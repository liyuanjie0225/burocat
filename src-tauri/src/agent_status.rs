//! AI agent task status: coding agents report turn lifecycle events to a loopback-only
//! endpoint, the pet shows a "working" pill while tasks run and announces completion.
//!
//! Pipeline: agent hook → `KuroCat --agent-hook <source>` (the entry is written into the
//! agent's own config by `agent_hooks`) → `POST http://127.0.0.1:27422/agent-event` →
//! `AgentRegistry` → `kuro-agent-status` event to the webviews.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager};

use crate::agent_hooks::{self, HookState};
use crate::config::AppSettings;
use crate::AppState;

/// Fixed port: hook entries must not depend on any setting.
pub const AGENT_PORT: u16 = 27422;

/// How long a turn that never reported an end stays known, so a late `Stop` can still be
/// announced with its title and duration. Interrupted turns never report `Stop` at all;
/// whether a quiet turn still shows in the pill is the frontend's decision.
const TRACK_TURNS_FOR_MS: u64 = 12 * 60 * 60 * 1000;

/// Upper bound on sessions kept in memory; the one quiet for longest goes first.
const TRACKED_TASKS: usize = 256;

/// Hook payloads can embed whole tool outputs; only the tool name is read from them.
const MAX_HOOK_STDIN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BODY_BYTES: u64 = 256 * 1024;
const MAX_REQUEST_BYTES: u64 = MAX_BODY_BYTES + 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const IO_TIMEOUT: Duration = Duration::from_millis(300);
const RECENT_EVENTS: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentEvent {
    /// `zcode`, `cursor`, … (free-form, slugified).
    pub source: String,
    /// `info` | `start` | `tool` | `permission` | `done`.
    pub event: String,
    /// Opaque agent-side session id; one task per source + session.
    pub session: String,
    /// Short display title, e.g. the first words of the prompt.
    pub label: String,
    /// Only `info` events carry it; the turn in progress inherits it.
    pub model: String,
    /// Excerpt of the final reply (on `done`).
    pub message: String,
    pub tool: String,
    /// On `done`: `interrupted`, or anything else for a normal completion.
    pub outcome: String,
}

impl AgentEvent {
    /// Keeps only what the pet can display, bounded in length. `None` when unusable.
    fn sanitized(mut self) -> Option<Self> {
        self.source = slug(&self.source, 24);
        if self.source.is_empty() {
            self.source = "agent".to_string();
        }
        self.session = slug(&self.session, 128);
        if self.session.is_empty() {
            return None;
        }
        self.event = self.event.trim().to_ascii_lowercase();
        if !matches!(self.event.as_str(), "info" | "start" | "tool" | "permission" | "done") {
            return None;
        }
        self.label = one_line(&self.label, 60);
        self.model = one_line(&self.model, 80);
        self.message = one_line(&self.message, 300);
        self.tool = one_line(&self.tool, 60);
        self.outcome = slug(&self.outcome, 24);
        Some(self)
    }

    fn id(&self) -> String {
        format!("{}:{}", self.source, self.session)
    }
}

/// Session ids are opaque tokens, never prose: ASCII identifier characters only.
fn slug(raw: &str, max: usize) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .take(max)
        .collect()
}

/// Single line, control characters stripped, trimmed and cut to `max` characters.
fn one_line(raw: &str, max: usize) -> String {
    raw.chars()
        .map(|ch| if ch.is_whitespace() || ch.is_control() { ' ' } else { ch })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn text<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn truthy(value: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| value.get(*key).and_then(Value::as_bool) == Some(true))
}

/// Maps the payload an agent writes to the hook's stdin onto the normalized event.
/// ZCode uses Claude-Code-style keys: snake_case aliases next to its camelCase fields,
/// with the session id and project dir also exported as environment variables.
pub fn hook_payload_to_event(
    source: &str,
    raw: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<AgentEvent> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let (event, outcome) = match text(&value, &["hook_event_name", "hookEventName"])? {
        "UserPromptSubmit" => ("start", ""),
        "PermissionRequest" => ("permission", ""),
        "Stop" => ("done", ""),
        // A tool cut short by the user is the only interruption an agent reports.
        "PostToolUseFailure" if truthy(&value, &["is_interrupt", "isInterrupt"]) => {
            ("done", "interrupted")
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => ("tool", ""),
        // Runs once per session and on compaction: neither starts nor ends a turn.
        "SessionStart" => ("info", ""),
        _ => return None,
    };
    let mut session = text(&value, &["session_id", "sessionId"])
        .unwrap_or_default()
        .to_string();
    if session.is_empty() {
        session = ["ZCODE_SESSION_ID", "CLAUDE_SESSION_ID"]
            .into_iter()
            .find_map(|key| env(key))
            .unwrap_or_default();
    }
    Some(AgentEvent {
        source: source.to_string(),
        event: event.to_string(),
        session,
        label: text(&value, &["prompt"]).unwrap_or_default().to_string(),
        model: text(&value, &["model"]).unwrap_or_default().to_string(),
        message: text(&value, &["last_assistant_message", "lastAssistantMessage"])
            .unwrap_or_default()
            .to_string(),
        tool: text(&value, &["tool_name", "toolName"])
            .unwrap_or_default()
            .to_string(),
        outcome: outcome.to_string(),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTask {
    pub id: String,
    pub source: String,
    pub label: String,
    pub model: String,
    /// Blocked on something only the user can answer, e.g. a permission prompt.
    pub waiting: bool,
    /// The tool that prompt is about, while waiting.
    pub tool: String,
    /// Tool calls finished so far in this turn: the only progress an agent reports.
    pub tools: u32,
    pub started_ms: u64,
    pub last_event_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentDone {
    pub id: String,
    pub source: String,
    pub label: String,
    pub model: String,
    /// Zero when the turn started while the pet was not running.
    pub elapsed_ms: u64,
    pub interrupted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    /// Active tasks, the ones waiting for the user first, then oldest start first.
    pub tasks: Vec<AgentTask>,
    pub running: usize,
    pub waiting: usize,
    /// Only set on the snapshot that ends a task.
    pub last_done: Option<AgentDone>,
    pub seq: u64,
}

/// What the pet window says it is rendering, as reported back by the frontend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUiState {
    /// Text of the pill, `None` while it is hidden.
    pub pill: Option<String>,
    pub waiting: bool,
    /// The last notice the window raised (a turn ended, or one needs the user).
    pub last_notice: Option<String>,
    pub updated_ms: u64,
}

/// One task per agent session; the turn in progress is what gets tracked.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    tasks: HashMap<String, AgentTask>,
    /// The task that ended most recently, for `GET /agent-state` diagnostics.
    last_done: Option<AgentDone>,
    ui: AgentUiState,
    /// Models learned from `info` events, keyed by task id, for turns that start later.
    models: HashMap<String, String>,
    /// The last few normalized events, for `GET /agent-state` diagnostics.
    recent: VecDeque<Value>,
    seq: u64,
}

/// How many sessions remember their model; anything older is forgotten first.
const KNOWN_MODELS: usize = 128;

fn new_task(event: &AgentEvent, now: u64) -> AgentTask {
    AgentTask {
        id: event.id(),
        source: event.source.clone(),
        label: String::new(),
        model: String::new(),
        waiting: false,
        tool: String::new(),
        tools: 0,
        started_ms: now,
        last_event_ms: now,
    }
}

impl AgentRegistry {
    /// Applies one event and returns the resulting snapshot, plus the task on `done`.
    fn apply(&mut self, event: &AgentEvent, now: u64) -> (AgentSnapshot, Option<AgentDone>) {
        if self.recent.len() == RECENT_EVENTS {
            self.recent.pop_front();
        }
        self.recent
            .push_back(serde_json::to_value(event).unwrap_or(Value::Null));
        self.seq += 1;

        let id = event.id();
        let done = match event.event.as_str() {
            "info" => {
                if !event.model.is_empty() {
                    self.forget_one_model();
                    self.models.insert(id, event.model.clone());
                }
                None
            }
            "start" => {
                let mut task = new_task(event, now);
                task.model = self.models.get(&id).cloned().unwrap_or_default();
                let task = self.tasks.entry(id).or_insert(task);
                // A new turn restarts the clock; whatever prompt was pending is answered.
                task.waiting = false;
                task.tool.clear();
                task.tools = 0;
                task.started_ms = now;
                task.last_event_ms = now;
                if !event.label.is_empty() {
                    task.label = event.label.clone();
                }
                if !event.model.is_empty() {
                    task.model = event.model.clone();
                }
                None
            }
            "tool" => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    // A finished tool also means an earlier prompt was answered.
                    task.waiting = false;
                    task.tool.clear();
                    task.tools = task.tools.saturating_add(1);
                    task.last_event_ms = now;
                }
                None
            }
            "permission" => {
                let task = self.tasks.entry(id).or_insert_with(|| new_task(event, now));
                task.waiting = true;
                task.tool = event.tool.clone();
                task.last_event_ms = now;
                None
            }
            "done" => {
                // Always announce: the turn may have started while the pet was not running.
                let task = self.tasks.remove(&id);
                let or_task = |value: &str, field: fn(&AgentTask) -> &String| {
                    if value.is_empty() {
                        task.as_ref().map(field).cloned().unwrap_or_default()
                    } else {
                        value.to_string()
                    }
                };
                Some(AgentDone {
                    // `Stop` carries neither the prompt nor the model: both live on the task.
                    label: or_task(&event.label, |task| &task.label),
                    model: or_task(&event.model, |task| &task.model),
                    elapsed_ms: task
                        .as_ref()
                        .map_or(0, |task| now.saturating_sub(task.started_ms)),
                    interrupted: event.outcome == "interrupted",
                    id,
                    source: event.source.clone(),
                })
            }
            _ => None,
        };
        if let Some(done) = &done {
            self.last_done = Some(done.clone());
        }
        self.evict(now);
        (self.snapshot(done.as_ref()), done)
    }

    fn forget_one_model(&mut self) {
        if self.models.len() >= KNOWN_MODELS {
            // A HashMap has no order, so which session is forgotten is arbitrary.
            if let Some(key) = self.models.keys().next().cloned() {
                self.models.remove(&key);
            }
        }
    }

    /// Bounds memory: turns silent for very long go first, then the quietest ones left.
    fn evict(&mut self, now: u64) {
        self.tasks.retain(|_, task| {
            now.saturating_sub(task.last_event_ms) < TRACK_TURNS_FOR_MS
        });
        while self.tasks.len() > TRACKED_TASKS {
            let quietest = self
                .tasks
                .iter()
                .min_by_key(|(_, task)| task.last_event_ms)
                .map(|(id, _)| id.clone());
            match quietest {
                Some(id) => {
                    self.tasks.remove(&id);
                }
                None => break,
            }
        }
    }

    fn snapshot(&self, last_done: Option<&AgentDone>) -> AgentSnapshot {
        let mut tasks: Vec<AgentTask> = self.tasks.values().cloned().collect();
        tasks.sort_by_key(|task| (!task.waiting, task.started_ms));
        AgentSnapshot {
            running: tasks.iter().filter(|task| !task.waiting).count(),
            waiting: tasks.iter().filter(|task| task.waiting).count(),
            tasks,
            last_done: last_done.cloned(),
            seq: self.seq,
        }
    }
}

static SERVER_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    enabled: bool,
    auto_install: bool,
    exe: String,
}

static APPLIED: Mutex<Option<Applied>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn settings(app: &AppHandle) -> MutexGuard<'_, AppSettings> {
    lock(&app.state::<AppState>().inner().settings)
}

fn registry(app: &AppHandle) -> MutexGuard<'_, AgentRegistry> {
    lock(app.state::<Mutex<AgentRegistry>>().inner())
}

fn current_exe() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Registers the registry, starts the loopback endpoint and wires up the agents' hooks
/// for the current settings. Called once from `setup`.
pub fn init(app: &AppHandle) {
    app.manage(Mutex::new(AgentRegistry::default()));
    sync(app, &settings(app).clone());
}

/// Brings the endpoint and the agents' hook entries in line with `settings`. A no-op
/// (no thread, no file access) while nothing relevant changed.
pub fn sync(app: &AppHandle, settings: &AppSettings) {
    if settings.agent_status_enabled && !SERVER_STARTED.swap(true, Ordering::SeqCst) {
        let handle = app.clone();
        std::thread::spawn(move || serve(handle));
    }

    let next = Applied {
        enabled: settings.agent_status_enabled,
        auto_install: settings.agent_hook_auto_install,
        exe: current_exe(),
    };
    let unchanged = {
        let mut applied = lock(&APPLIED);
        let unchanged = *applied == Some(next.clone());
        if !unchanged {
            *applied = Some(next.clone());
        }
        unchanged
    };
    if unchanged {
        return;
    }
    let Some(path) = agent_hooks::zcode_config_path() else {
        return;
    };
    // Switching the feature off takes our entries back out, so no hook fires for nothing.
    let outcome = if next.enabled && next.auto_install && !next.exe.is_empty() {
        agent_hooks::install_zcode(&path, &next.exe)
    } else {
        agent_hooks::uninstall_zcode(&path)
    };
    if let Err(error) = outcome {
        eprintln!("KuroCat agent hooks: {error}");
    }
}

#[tauri::command]
pub fn get_agent_hook_info() -> HookState {
    agent_hooks::zcode_state(&current_exe())
}

/// The pet window asks for this on load, so it shows what is already running instead of
/// waiting blind for the next event.
#[tauri::command]
pub fn get_agent_snapshot(app: AppHandle) -> AgentSnapshot {
    registry(&app).snapshot(None)
}

/// The pet window reports what it actually renders, so `GET /agent-state` can tell
/// "event accepted" apart from "event shown".
#[tauri::command]
pub fn agent_ui_reported(app: AppHandle, pill: Option<String>, waiting: bool, notice: Option<String>) {
    let short = |text: String| {
        let text = one_line(&text, 120);
        (!text.is_empty()).then_some(text)
    };
    let mut registry = registry(&app);
    registry.ui.pill = pill.and_then(short);
    registry.ui.waiting = waiting;
    registry.ui.last_notice = notice.and_then(short).or(registry.ui.last_notice.clone());
    registry.ui.updated_ms = now_ms();
}

fn serve(app: AppHandle) {
    let address = SocketAddr::from(([127, 0, 0, 1], AGENT_PORT));
    let Ok(listener) = TcpListener::bind(address) else {
        // Let a later `sync` try again instead of remembering a broken state.
        SERVER_STARTED.store(false, Ordering::SeqCst);
        return;
    };
    for stream in listener.incoming().flatten() {
        let app = app.clone();
        std::thread::spawn(move || handle_conn(stream, &app));
    }
}

fn respond(writer: &mut TcpStream, status: &str, body: &str) {
    // Deliberately no CORS headers: web pages get neither reads nor writes.
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = writer.write_all(response.as_bytes());
    let _ = writer.flush();
}

struct Request {
    method: String,
    path: String,
    content_type: String,
    /// Browsers always send `Origin` on a cross-origin POST; our hook never does.
    from_browser: bool,
    body: Vec<u8>,
}

fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();

    let mut content_type = String::new();
    let mut from_browser = false;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.to_ascii_lowercase().as_str() {
            "content-type" => content_type = value.trim().to_ascii_lowercase(),
            "content-length" => content_length = value.trim().parse().ok()?,
            "origin" => from_browser = true,
            _ => {}
        }
    }
    if content_length > MAX_BODY_BYTES as usize {
        return None;
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        path,
        content_type,
        from_browser,
        body,
    })
}

fn handle_conn(stream: TcpStream, app: &AppHandle) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));

    let Some(request) = read_request(&mut reader) else {
        return respond(&mut writer, "400 Bad Request", r#"{"error":"Malformed request"}"#);
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/agent-state") => respond(&mut writer, "200 OK", &state_json(app)),
        ("POST", "/agent-event") => handle_event(app, &request, &mut writer),
        _ => respond(
            &mut writer,
            "404 Not Found",
            r#"{"error":"Use POST /agent-event or GET /agent-state"}"#,
        ),
    }
}

fn handle_event(app: &AppHandle, request: &Request, writer: &mut TcpStream) {
    if !settings(app).agent_status_enabled {
        return respond(
            writer,
            "403 Forbidden",
            r#"{"error":"Agent status is disabled in KuroCat settings"}"#,
        );
    }
    // A JSON content type plus "no Origin" keeps web pages from driving the pet through
    // a browser: together they force a CORS preflight that this server never answers.
    if request.from_browser || !request.content_type.starts_with("application/json") {
        return respond(
            writer,
            "415 Unsupported Media Type",
            r#"{"error":"Expected application/json and no Origin header"}"#,
        );
    }
    let event: AgentEvent =
        match serde_json::from_slice(&request.body).ok().and_then(AgentEvent::sanitized) {
            Some(event) => event,
            None => return respond(writer, "400 Bad Request", r#"{"error":"Unknown agent event"}"#),
        };

    let now = now_ms();
    let (snapshot, done) = registry(app).apply(&event, now);
    let completed = done.as_ref().is_some_and(|done| !done.interrupted);
    if completed && settings(app).sound_enabled {
        play_done_sound();
    }
    let _ = app.emit("kuro-agent-status", &snapshot);
    respond(
        writer,
        "200 OK",
        &serde_json::to_string(&snapshot).unwrap_or_default(),
    );
}

fn state_json(app: &AppHandle) -> String {
    let (snapshot, recent, last_finished, ui) = {
        let registry = registry(app);
        (
            registry.snapshot(None),
            registry.recent.clone(),
            registry.last_done.clone(),
            registry.ui.clone(),
        )
    };
    // "ui" is what the pet window reports rendering; the rest is what the backend knows.
    let state = json!({
        "enabled": settings(app).agent_status_enabled,
        "port": AGENT_PORT,
        "snapshot": snapshot,
        "lastFinished": last_finished,
        "recentEvents": recent,
        "ui": ui,
        "hooks": agent_hooks::zcode_state(&current_exe()),
    });
    serde_json::to_string(&state).unwrap_or_default()
}

/// The platform's own notification sound: nothing bundled, no extra dependency.
fn play_done_sound() {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        // `afplay` ships with macOS; stay silent when it is missing or has no output.
        if let Ok(mut child) = Command::new("/usr/bin/afplay")
            .arg("/System/Library/Sounds/Glass.aiff")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            // Reap the player so completions never pile up as zombie processes.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    #[cfg(windows)]
    {
        // windows-sys files MessageBeep under Diagnostics::Debug, not next to its constants.
        // SAFETY: MessageBeep has no preconditions.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(
                windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK,
            );
        }
    }
}

/// `KuroCat --agent-hook <source>`: the command agents run. Forwards the hook payload
/// from stdin to the running pet. Always exits 0 and never writes to stdout, because
/// agents treat any other exit code or unexpected output as a hook failure.
pub fn run_agent_hook(source: &str) -> ! {
    forward_hook(source);
    std::process::exit(0)
}

fn forward_hook(source: &str) {
    let mut raw = String::new();
    if std::io::stdin()
        .take(MAX_HOOK_STDIN_BYTES)
        .read_to_string(&mut raw)
        .is_err()
    {
        return;
    }
    let getenv = |name: &str| std::env::var(name).ok();
    let Some(event) = hook_payload_to_event(source, &raw, &getenv).and_then(AgentEvent::sanitized)
    else {
        return;
    };
    let Ok(body) = serde_json::to_string(&event) else {
        return;
    };
    let _ = post_event(&body);
}

fn post_event(body: &str) -> std::io::Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], AGENT_PORT));
    // A pet that is not running stays cheap: a refused loopback connect returns at once.
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let request = format!(
        "POST /agent-event HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    // Wait for the reply so the pet has taken the event in before this process vanishes.
    let mut status_line = String::new();
    let _ = BufReader::new(stream).read_line(&mut status_line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "sess_ff43427e-3df3-4c00-88c0-f60cd4567054";
    const NO_ENV: fn(&str) -> Option<String> = |_| None;

    fn hook_event(kind: &str, session: &str) -> AgentEvent {
        AgentEvent {
            source: "zcode".into(),
            event: kind.into(),
            session: session.into(),
            ..AgentEvent::default()
        }
        .sanitized()
        .expect("event should be usable")
    }

    fn start(session: &str) -> AgentEvent {
        AgentEvent {
            label: "帮我开发一下吧".into(),
            model: "account:bigmodel/GLM-5.4".into(),
            ..hook_event("start", session)
        }
    }

    fn apply(
        registry: &mut AgentRegistry,
        event: AgentEvent,
        now: u64,
    ) -> (AgentSnapshot, Option<AgentDone>) {
        registry.apply(&event, now)
    }

    #[test]
    fn a_turn_goes_running_waiting_then_done_with_its_elapsed_time() {
        let mut registry = AgentRegistry::default();
        assert!(apply(&mut registry, start(SESSION), 1_000).1.is_none());
        let (snapshot, _) = apply(
            &mut registry,
            AgentEvent {
                tool: "Bash".into(),
                ..hook_event("permission", SESSION)
            },
            4_000,
        );
        assert_eq!((snapshot.running, snapshot.waiting), (0, 1));
        assert_eq!(snapshot.tasks[0].waiting, true);
        assert_eq!(snapshot.tasks[0].tool, "Bash", "the prompt says what it is waiting for");
        assert_eq!(snapshot.tasks[0].label, "帮我开发一下吧", "the prompt stays the title");
        assert_eq!(snapshot.tasks[0].model, "account:bigmodel/GLM-5.4");

        let (snapshot, done) = apply(&mut registry, hook_event("done", SESSION), 61_000);
        let done = done.expect("done finishes the task");
        assert_eq!(done.elapsed_ms, 60_000, "measured from the start of the turn");
        assert!(!done.interrupted);
        assert_eq!(done.label, "帮我开发一下吧", "the title outlives the task");
        assert_eq!(done.model, "account:bigmodel/GLM-5.4");
        assert!(snapshot.tasks.is_empty());
        assert_eq!(snapshot.last_done.as_ref(), Some(&done), "only this snapshot carries it");
        assert_eq!(registry.last_done.as_ref(), Some(&done), "but the registry remembers it");
        assert!(apply(&mut registry, start("other"), 70_000).0.last_done.is_none());
    }

    #[test]
    fn a_new_turn_clears_an_answered_prompt_and_restarts_the_clock() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start(SESSION), 1_000);
        apply(&mut registry, hook_event("permission", SESSION), 2_000);
        let (snapshot, _) = apply(&mut registry, start(SESSION), 200_000);
        assert_eq!(snapshot.waiting, 0, "a new prompt means the old one was answered");
        assert_eq!(snapshot.tasks.len(), 1, "a session is only ever one task");
        assert_eq!(snapshot.tasks[0].started_ms, 200_000);
        let (_, done) = apply(&mut registry, hook_event("done", SESSION), 200_500);
        assert_eq!(done.expect("done").elapsed_ms, 500);
    }

    #[test]
    fn a_failed_tool_reports_activity_and_clears_the_wait() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start(SESSION), 1_000);
        apply(&mut registry, hook_event("permission", SESSION), 2_000);
        let (snapshot, done) = apply(&mut registry, hook_event("tool", SESSION), 3_000);
        assert!(done.is_none(), "a failed tool does not end the turn");
        assert_eq!(snapshot.waiting, 0, "the prompt was answered (denied)");
        assert_eq!(snapshot.tasks[0].last_event_ms, 3_000);
    }

    #[test]
    fn finished_tools_are_the_progress_of_a_turn_and_reset_with_the_next_one() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start(SESSION), 1_000);
        for step in 1..=12u64 {
            let (snapshot, _) = apply(
                &mut registry,
                AgentEvent {
                    tool: "Bash".into(),
                    ..hook_event("tool", SESSION)
                },
                1_000 + step * 10_000,
            );
            assert_eq!(snapshot.tasks[0].tools, step as u32);
            assert_eq!(snapshot.tasks[0].started_ms, 1_000, "the clock still starts at the prompt");
            assert_eq!(snapshot.tasks[0].last_event_ms, 1_000 + step * 10_000);
        }
        // A tool of a turn the pet never saw start cannot invent that turn…
        let (snapshot, _) = apply(&mut registry, hook_event("tool", "unknown-session"), 200_000);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, format!("zcode:{SESSION}"));

        let (snapshot, _) = apply(&mut registry, start(SESSION), 300_000);
        assert_eq!(snapshot.tasks[0].tools, 0, "a new prompt starts from zero");
    }

    #[test]
    fn sessions_are_tracked_independently_and_waiting_ones_are_listed_first() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start("aaa"), 10);
        apply(&mut registry, start("bbb"), 20);
        apply(&mut registry, hook_event("permission", "bbb"), 30);
        let (snapshot, _) = apply(&mut registry, hook_event("done", "zzz"), 40);
        assert_eq!((snapshot.running, snapshot.waiting), (1, 1));
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["zcode:bbb", "zcode:aaa"], "waiting first, then oldest start");
    }

    #[test]
    fn an_interrupted_tool_ends_the_turn_as_interrupted() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start(SESSION), 1_000);
        let (snapshot, done) = apply(
            &mut registry,
            AgentEvent {
                outcome: "interrupted".into(),
                ..hook_event("done", SESSION)
            },
            2_000,
        );
        let done = done.expect("done");
        assert!(done.interrupted);
        assert_eq!(done.elapsed_ms, 1_000);
        assert!(snapshot.tasks.is_empty());
    }

    #[test]
    fn a_done_without_a_start_still_announces_with_no_duration() {
        let (snapshot, done) = apply(&mut AgentRegistry::default(), hook_event("done", SESSION), 5_000);
        let done = done.expect("done");
        assert_eq!(done.elapsed_ms, 0);
        assert_eq!(done.label, "");
        assert_eq!(snapshot.seq, 1);
    }

    #[test]
    fn a_quiet_turn_stays_known_for_hours_so_a_late_stop_keeps_its_title() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start(SESSION), 1_000);
        // Forty minutes into one long tool call: nothing was reported since the prompt.
        let (snapshot, done) = apply(&mut registry, hook_event("done", SESSION), 2_401_000);
        assert!(snapshot.tasks.is_empty());
        let done = done.expect("done");
        assert_eq!(done.label, "帮我开发一下吧");
        assert_eq!(done.elapsed_ms, 2_400_000);
    }

    #[test]
    fn turns_are_forgotten_when_very_old_or_when_they_pile_up() {
        let mut registry = AgentRegistry::default();
        apply(&mut registry, start("ancient"), 0);
        apply(&mut registry, start("old"), TRACK_TURNS_FOR_MS + 10);
        let (snapshot, _) = apply(&mut registry, start("new"), TRACK_TURNS_FOR_MS + 20);
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["zcode:old", "zcode:new"], "silent for half a day means forgotten");

        let mut registry = AgentRegistry::default();
        for index in 0..(TRACKED_TASKS + 10) {
            apply(&mut registry, start(&format!("s{index}")), index as u64);
        }
        let (snapshot, _) = apply(&mut registry, start("latest"), 999_999);
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids.len(), TRACKED_TASKS, "memory stays bounded");
        assert_eq!(ids.iter().any(|id| *id == "zcode:s0"), false, "the quietst turn went first");
        assert_eq!(ids.last(), Some(&"zcode:latest"), "what just reported always stays");
    }

    #[test]
    fn the_model_learned_at_session_start_sticks_to_its_turns() {
        let mut registry = AgentRegistry::default();
        let session_start = AgentEvent {
            model: "account:bigmodel/GLM-5.4".into(),
            ..hook_event("info", SESSION)
        };
        let (snapshot, _) = apply(&mut registry, session_start, 500);
        assert!(snapshot.tasks.is_empty(), "session start is not a turn");

        let (snapshot, _) = apply(&mut registry, start(SESSION), 1_000);
        assert_eq!(snapshot.tasks[0].model, "account:bigmodel/GLM-5.4");

        let (snapshot, done) = apply(&mut registry, hook_event("done", SESSION), 2_000);
        assert_eq!(done.expect("done").model, "account:bigmodel/GLM-5.4");
        assert!(snapshot.tasks.is_empty());

        // The next turn of the same session still knows it…
        let (snapshot, _) = apply(&mut registry, start(SESSION), 3_000);
        assert_eq!(snapshot.tasks[0].model, "account:bigmodel/GLM-5.4");
        // …unless the event itself names another one.
        let (snapshot, _) = apply(
            &mut registry,
            AgentEvent {
                model: "glm/GLM-5.3-Flash".into(),
                ..start(SESSION)
            },
            4_000,
        );
        assert_eq!(snapshot.tasks[0].model, "glm/GLM-5.3-Flash");
    }

    #[test]
    fn remembered_models_stay_bounded() {
        let mut registry = AgentRegistry::default();
        for index in 0..(KNOWN_MODELS + 40) {
            apply(
                &mut registry,
                AgentEvent {
                    model: format!("m{index}"),
                    ..hook_event("info", &format!("s{index}"))
                },
                index as u64,
            );
        }
        assert_eq!(registry.models.len(), KNOWN_MODELS);
    }

    #[test]
    fn the_diagnostic_ring_keeps_only_the_most_recent_events() {
        let mut registry = AgentRegistry::default();
        for index in 0..(RECENT_EVENTS as u64 + 5) {
            apply(&mut registry, hook_event("done", &format!("s{index}")), index);
        }
        let recent: Vec<&str> = registry
            .recent
            .iter()
            .map(|event| event["session"].as_str().expect("session"))
            .collect();
        assert_eq!(recent.len(), RECENT_EVENTS);
        assert_eq!((recent.first(), recent.last()), (Some(&"s5"), Some(&"s44")));
    }

    #[test]
    fn zcode_hook_payloads_map_onto_the_event_with_env_as_a_fallback() {
        let prompt_submit = r#"{"agentName":"zcode-agent","cwd":"/repo","hookEventName":"UserPromptSubmit","mode":"guarded","model":"glm/GLM-5.4","prompt":"  修复\n登录 bug  ","sessionId":"sess_1","timestamp":"2026-11-25T10:00:00.000Z","toolCallCount":0,"hook_event_name":"UserPromptSubmit","permission_mode":"guarded","session_id":"sess_1"}"#;
        let event = hook_payload_to_event("zcode", prompt_submit, &NO_ENV).expect("event");
        assert_eq!(
            event,
            AgentEvent {
                source: "zcode".into(),
                event: "start".into(),
                session: "sess_1".into(),
                label: "  修复\n登录 bug  ".into(),
                model: "glm/GLM-5.4".into(),
                ..AgentEvent::default()
            }
        );
        assert_eq!(
            event.sanitized().expect("usable").label,
            "修复 登录 bug",
            "the title is one trimmed line"
        );

        let stop = r#"{"hook_event_name":"Stop","session_id":"sess_1","last_assistant_message":"已修复并验证。","stop_hook_active":false}"#;
        let event = hook_payload_to_event("zcode", stop, &NO_ENV).expect("event");
        assert_eq!(event.event, "done");
        assert_eq!(event.message, "已修复并验证。");

        let env_only = r#"{"hook_event_name":"Stop"}"#;
        let event = hook_payload_to_event("zcode", env_only, &|name| {
            (name == "ZCODE_SESSION_ID").then(|| "sess_env".to_string())
        })
        .expect("event");
        assert_eq!(event.session, "sess_env");

        let permission = r#"{"hook_event_name":"PermissionRequest","session_id":"sess_1","tool_name":"Bash","tool_input":{"command":"npm test"}}"#;
        let event = hook_payload_to_event("zcode", permission, &NO_ENV).expect("event");
        assert_eq!((event.event.as_str(), event.tool.as_str()), ("permission", "Bash"));
    }

    #[test]
    fn only_an_interrupted_tool_failure_ends_the_turn() {
        let interrupted = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","tool_name":"Bash","is_interrupt":true}"#;
        let event = hook_payload_to_event("zcode", interrupted, &NO_ENV).expect("event");
        assert_eq!((event.event.as_str(), event.outcome.as_str()), ("done", "interrupted"));

        let denied = r#"{"hook_event_name":"PostToolUseFailure","session_id":"s","tool_name":"Bash","is_interrupt":false,"error":"denied"}"#;
        let event = hook_payload_to_event("zcode", denied, &NO_ENV).expect("event");
        assert_eq!((event.event.as_str(), event.outcome.as_str()), ("tool", ""));

        let tool = r#"{"hook_event_name":"PostToolUse","session_id":"s","tool_name":"Bash"}"#;
        assert_eq!(hook_payload_to_event("zcode", tool, &NO_ENV).expect("event").event, "tool");

        let session_start = r#"{"hook_event_name":"SessionStart","session_id":"s","source":"startup","model":"glm/GLM-5.4"}"#;
        let event = hook_payload_to_event("zcode", session_start, &NO_ENV).expect("event");
        assert_eq!((event.event.as_str(), event.model.as_str()), ("info", "glm/GLM-5.4"));
        let without_model = r#"{"hook_event_name":"SessionStart","session_id":"s","source":"compact"}"#;
        assert_eq!(hook_payload_to_event("zcode", without_model, &NO_ENV).expect("event").event, "info");

        for ignored in [
            r#"{"hook_event_name":"Whatever","session_id":"s"}"#,
            r#"{"event":"Stop","session_id":"s"}"#,
            r#"not json"#,
            "",
        ] {
            assert!(hook_payload_to_event("zcode", ignored, &NO_ENV).is_none(), "{ignored}");
        }
        // A payload without any session id maps fine, but can never be attributed to a task.
        let no_session = r#"{"hook_event_name":"Stop","session_id":""}"#;
        assert!(
            hook_payload_to_event("zcode", no_session, &NO_ENV)
                .and_then(AgentEvent::sanitized)
                .is_none()
        );
    }

    #[test]
    fn unusable_events_are_rejected_and_text_is_bounded() {
        let long = "很".repeat(500);
        let event = AgentEvent {
            source: "Z Code!".into(),
            event: " DONE ".into(),
            session: format!("{SESSION} /*?"),
            label: long.clone(),
            model: long.clone(),
            message: long,
            tool: "Bash".into(),
            outcome: "Interrupted".into(),
        }
        .sanitized()
        .expect("usable");
        assert_eq!(event.source, "ZCode", "only identifier characters survive");
        assert_eq!(event.event, "done");
        assert_eq!(event.session, SESSION, "the id is only its identifier characters");
        assert_eq!(event.label.chars().count(), 60);
        assert_eq!(event.model.chars().count(), 80);
        assert_eq!(event.message.chars().count(), 300);
        assert_eq!(event.outcome, "Interrupted", "case is kept: it is a value, not an id");

        let unusable = [
            AgentEvent::default(),
            AgentEvent {
                session: "///".into(),
                ..hook_event("done", SESSION)
            },
            AgentEvent {
                event: "party".into(),
                ..hook_event("done", SESSION)
            },
            AgentEvent {
                event: " ".into(),
                ..hook_event("done", SESSION)
            },
        ];
        for raw in &unusable {
            assert!(raw.clone().sanitized().is_none(), "{raw:?}");
        }
    }

    #[test]
    fn requests_are_parsed_and_oversized_or_truncated_ones_refused() {
        let raw = "POST /agent-event?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: Application/JSON; charset=utf-8\r\nContent-Length: 16\r\nOrigin: https://evil.test\r\n\r\n{\"event\":\"done\"}";
        let request = read_request(&mut BufReader::new(raw.as_bytes())).expect("request");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/agent-event", "the query string is not part of the route");
        assert_eq!(request.content_type, "application/json; charset=utf-8");
        assert!(request.from_browser);
        assert_eq!(request.body, b"{\"event\":\"done\"}");

        let oversized = format!(
            "POST /agent-event HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let truncated_body = "POST /agent-event HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}";
        let without_path = "GET\r\n\r\n";
        let garbage = "\r\n";
        for bad in [oversized.as_str(), truncated_body, without_path, garbage] {
            assert!(read_request(&mut BufReader::new(bad.as_bytes())).is_none(), "{bad:?}");
        }
    }
}

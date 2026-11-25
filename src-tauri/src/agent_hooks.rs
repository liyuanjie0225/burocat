//! Writes and removes this app's own hook entries in coding agents' config files, so
//! agents report task lifecycle events without any manual JSON editing.
//!
//! Every change is a read → merge → atomic rename over the agent's config. Unknown keys
//! and the user's own hooks are never touched, an entry only counts as "ours" when it
//! carries the `--agent-hook` argument, and a file is left completely alone when it is
//! not valid JSON or when the agent's hooks were switched off explicitly.

use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Lifecycle events subscribed by default.
///
/// Five are rare (a handful per turn): `SessionStart` fires once per session and on
/// compaction and is the only one that names the model; `PostToolUseFailure` only fires
/// when a tool fails, is denied or interrupted. `PostToolUse` is the one exception: it
/// fires after every tool call, and is what makes progress visible at all — an agent
/// reports nothing else while it works, and a turn the user interrupts is never
/// reported as ended without it.
pub const ZCODE_EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "PermissionRequest",
    "PostToolUseFailure",
    "Stop",
];

pub const HOOK_FLAG: &str = "--agent-hook";

/// Shown by the agent while the hook runs, and a readable signature of our entries.
const HOOK_STATUS: &str = "KuroCat";

/// Generous for a loopback POST; only exists so a wedged hook can never stall a session.
const HOOK_TIMEOUT_MS: u64 = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookState {
    /// Whether the agent is installed on this machine (its config directory exists).
    pub detected: bool,
    /// Whether our entries are present and point at the running executable.
    pub installed: bool,
    pub config_path: String,
    pub command: String,
    pub events: Vec<String>,
    /// Set when the config exists but could not be read or merged.
    pub error: Option<String>,
}

/// `~/.zcode/cli/config.json` — the user-level config that applies to every ZCode session.
pub fn zcode_config_path() -> Option<PathBuf> {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok()?;
    let dir = PathBuf::from(home).join(".zcode").join("cli");
    dir.is_dir().then(|| dir.join("config.json"))
}

fn desired_hook(exe: &str) -> Value {
    json!({
        "type": "process",
        "command": exe,
        "args": [HOOK_FLAG, "zcode"],
        "timeoutMs": HOOK_TIMEOUT_MS,
        "statusMessage": HOOK_STATUS,
    })
}

fn owns(item: &Value) -> bool {
    item.get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| args.iter().any(|arg| arg == HOOK_FLAG))
}

/// A group without a matcher fires for every occurrence of the event.
fn is_matcherless(group: &Value) -> bool {
    group.is_object()
        && group
            .get("matcher")
            .map_or(true, |matcher| matcher.as_str().is_some_and(str::is_empty))
}

/// Merges our ZCode hooks into a parsed config. Returns whether anything changed.
pub fn merge_zcode(root: &mut Value, exe: &str) -> Result<bool, String> {
    let desired = desired_hook(exe);
    let mut changed = false;

    let root = root
        .as_object_mut()
        .ok_or_else(|| "the config root is not a JSON object".to_string())?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| "`hooks` is not a JSON object".to_string())?;
    match hooks.get("enabled") {
        Some(Value::Bool(true)) => {}
        // An explicit opt-out is the user's decision; never flip it back.
        Some(Value::Bool(false)) => {
            return Err("hooks are switched off in the ZCode config (`enabled` is false)".into())
        }
        _ => {
            hooks.insert("enabled".into(), Value::Bool(true));
            changed = true;
        }
    }
    let events = hooks.entry("events").or_insert_with(|| json!({}));
    let events = events
        .as_object_mut()
        .ok_or_else(|| "`hooks.events` is not a JSON object".to_string())?;

    for event in ZCODE_EVENTS {
        let groups = events.entry(event).or_insert_with(|| json!([]));
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.events.{event}` is not a JSON array"))?;
        if !groups.iter().any(is_matcherless) {
            groups.insert(0, json!({ "hooks": [] }));
        }
        let index = groups.iter().position(is_matcherless).unwrap_or(0);
        let group = groups[index]
            .as_object_mut()
            .expect("is_matcherless only accepts objects");
        let list = group.entry("hooks").or_insert_with(|| json!([]));
        let list = list
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.events.{event}[].hooks` is not a JSON array"))?;
        match list.iter_mut().find(|item| owns(item)) {
            // Refresh an existing entry in place, so the user's own ordering survives.
            Some(item) => {
                if *item != desired {
                    *item = desired.clone();
                    changed = true;
                }
            }
            None => {
                list.push(desired.clone());
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Removes our ZCode hooks again. Returns whether anything changed.
pub fn remove_zcode(root: &mut Value) -> bool {
    let mut changed = false;
    let Some(hooks) = root
        .as_object_mut()
        .and_then(|root| root.get_mut("hooks"))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };

    if let Some(events) = hooks.get_mut("events").and_then(Value::as_object_mut) {
        for event in ZCODE_EVENTS {
            let Some(groups) = events.get_mut(event).and_then(Value::as_array_mut) else {
                continue;
            };
            for group in groups.iter_mut().filter(|group| is_matcherless(group)) {
                let Some(list) = group
                    .as_object_mut()
                    .and_then(|group| group.get_mut("hooks"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                let before = list.len();
                list.retain(|item| !owns(item));
                changed |= list.len() != before;
            }
            // Drop the group we created once nothing is left in it.
            groups.retain(|group| {
                !(is_matcherless(group)
                    && group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty))
            });
        }
    }
    if !changed {
        return false;
    }
    // Below this point only unwinding whatever adding our entries may have created.
    if let Some(events) = hooks.get_mut("events").and_then(Value::as_object_mut) {
        for event in ZCODE_EVENTS {
            let empty = events
                .get(event)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            if empty {
                events.remove(event);
            }
        }
        if events.is_empty() {
            hooks.remove("events");
        }
    }
    if hooks.get("events").is_none() {
        hooks.remove("enabled");
    }
    if hooks.is_empty() {
        if let Some(root) = root.as_object_mut() {
            root.remove("hooks");
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    Unchanged,
    Written,
}

/// Adds our ZCode hooks for the given executable, creating the config if needed.
pub fn install_zcode(path: &Path, exe: &str) -> Result<FileChange, String> {
    update_config(path, |root| merge_zcode(root, exe))
}

/// Removes our ZCode hooks. Other hooks and every unrelated key stay as they are.
pub fn uninstall_zcode(path: &Path) -> Result<FileChange, String> {
    if !path.exists() {
        return Ok(FileChange::Unchanged);
    }
    update_config(path, |root| Ok(remove_zcode(root)))
}

/// Whether ZCode is on this machine and wired up to the running executable.
pub fn zcode_state(exe: &str) -> HookState {
    match zcode_config_path() {
        Some(path) => state_at(&path, exe),
        None => HookState {
            detected: false,
            installed: false,
            config_path: String::new(),
            command: String::new(),
            events: ZCODE_EVENTS.iter().map(|event| (*event).to_string()).collect(),
            error: None,
        },
    }
}

fn state_at(path: &Path, exe: &str) -> HookState {
    let mut state = HookState {
        detected: true,
        installed: false,
        config_path: path.display().to_string(),
        command: format!("{exe} {HOOK_FLAG} zcode"),
        events: ZCODE_EVENTS.iter().map(|event| (*event).to_string()).collect(),
        error: None,
    };
    match read_config(path) {
        // Installed means "present and current": a merge that would change nothing.
        Ok(ConfigFile { mut root, .. }) => match merge_zcode(&mut root, exe) {
            Ok(stale) => state.installed = !stale,
            Err(error) => state.error = Some(error),
        },
        Err(error) => state.error = Some(error),
    }
    state
}

struct ConfigFile {
    /// As found on disk; a second read must still match right before the rename.
    raw: String,
    root: Value,
    trailing_newline: bool,
}

fn read_config(path: &Path) -> Result<ConfigFile, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigFile {
                raw: String::new(),
                root: json!({}),
                trailing_newline: true,
            })
        }
        Err(error) => return Err(error.to_string()),
    };
    let root = serde_json::from_str(&raw)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
    Ok(ConfigFile {
        trailing_newline: raw.ends_with('\n'),
        raw,
        root,
    })
}

fn write_config(path: &Path, file: &ConfigFile) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(&file.root).map_err(|error| error.to_string())?;
    if file.trailing_newline {
        text.push('\n');
    }
    let temporary = path.with_extension(format!("json.kurocat-{}.tmp", std::process::id()));
    let write = std::fs::write(&temporary, text).and_then(|()| {
        // Keep the agent's own permissions (its config is typically 0600).
        match std::fs::metadata(path) {
            Ok(meta) => std::fs::set_permissions(&temporary, meta.permissions()),
            // A config that did not exist before is created private.
            Err(_) => make_private(&temporary),
        }
    });
    let write = write.and_then(|()| std::fs::rename(&temporary, path));
    if write.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write.map_err(|error| error.to_string())
}

#[cfg(unix)]
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The agent rewrites its config itself: a merge is redone on top of a fresh copy when
/// the file changed underneath us, and nothing is written at all for a no-op merge.
fn update_config(
    path: &Path,
    merge: impl Fn(&mut Value) -> Result<bool, String>,
) -> Result<FileChange, String> {
    for attempt in 0..3 {
        let mut file = read_config(path)?;
        if !merge(&mut file.root)? {
            return Ok(FileChange::Unchanged);
        }
        if attempt == 2 || read_config(path)?.raw == file.raw {
            write_config(path, &file)?;
            return Ok(FileChange::Written);
        }
    }
    Err(format!("{} keeps changing; KuroCat gave up editing it", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/Applications/KuroCat.app/Contents/MacOS/KuroCat";
    const STALE_EXE: &str = "/old/KuroCat.app/Contents/MacOS/KuroCat";

    fn event_hooks<'a>(root: &'a Value, event: &str) -> &'a Vec<Value> {
        root["hooks"]["events"][event][0]["hooks"]
            .as_array()
            .expect("matcher-less group with a hooks list")
    }

    #[test]
    fn merge_adds_our_entries_and_enables_config_hooks() {
        let mut root = json!({ "plugins": { "enabledPlugins": { "a": true } } });
        assert!(merge_zcode(&mut root, EXE).expect("merge should succeed"));
        assert_eq!(root["hooks"]["enabled"], Value::Bool(true));
        for event in ZCODE_EVENTS {
            assert_eq!(root["hooks"]["events"][event].as_array().expect("groups").len(), 1);
            assert_eq!(event_hooks(&root, event), &vec![desired_hook(EXE)]);
        }
    }

    #[test]
    fn merge_is_idempotent_and_keeps_unrelated_keys_in_order() {
        let mut root = json!({ "plugins": { "enabledPlugins": { "a": true } } });
        merge_zcode(&mut root, EXE).expect("first merge should succeed");
        assert!(!merge_zcode(&mut root, EXE).expect("second merge should succeed"));
        assert_eq!(root["plugins"]["enabledPlugins"]["a"], Value::Bool(true));
        let text = serde_json::to_string_pretty(&root).expect("should serialize");
        assert!(text.starts_with("{\n  \"plugins\":"), "plugins should stay first");
    }

    #[test]
    fn merge_refreshes_a_stale_executable_path_without_duplicating() {
        let mut root = json!({});
        merge_zcode(&mut root, STALE_EXE).expect("merge with the old path");
        let mut user_hooks = event_hooks(&root, "Stop").clone();
        user_hooks.insert(0, json!({ "type": "command", "command": "echo user-hook" }));
        root["hooks"]["events"]["Stop"][0]["hooks"] = Value::Array(user_hooks);

        assert!(merge_zcode(&mut root, EXE).expect("merge should refresh"));
        let hooks = event_hooks(&root, "Stop");
        assert_eq!(hooks.len(), 2, "our entry is replaced, not duplicated");
        assert_eq!(hooks[0]["command"], "echo user-hook", "user hook keeps its place");
        assert_eq!(hooks[1], desired_hook(EXE));
    }

    #[test]
    fn merge_joins_a_matcher_less_group_but_never_a_matched_one() {
        let mut root = json!({
            "hooks": {
                "enabled": true,
                "events": {
                    "Stop": [
                        { "matcher": "done", "hooks": [{ "type": "command", "command": "echo matched" }] },
                        { "hooks": [{ "type": "command", "command": "echo shared" }] }
                    ]
                }
            }
        });
        assert!(merge_zcode(&mut root, EXE).expect("merge should succeed"));
        let groups = root["hooks"]["events"]["Stop"].as_array().expect("groups");
        assert_eq!(groups.len(), 2, "no extra group should be added");
        let matched = groups[0]["hooks"].as_array().expect("list");
        assert_eq!(matched.len(), 1, "a matched group is not ours to touch");
        let shared = groups[1]["hooks"].as_array().expect("list");
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0]["command"], "echo shared");
        assert_eq!(shared[1], desired_hook(EXE));
    }

    #[test]
    fn merge_refuses_an_explicit_opt_out_without_touching_the_file() {
        let mut root = json!({ "hooks": { "enabled": false, "events": { "Stop": [] } } });
        let before = root.clone();
        assert!(merge_zcode(&mut root, EXE).is_err());
        assert_eq!(root, before);
    }

    #[test]
    fn merge_refuses_unexpected_shapes() {
        for broken in [
            json!([]),
            json!({ "hooks": [] }),
            json!({ "hooks": { "events": [] } }),
            json!({ "hooks": { "events": { "Stop": {} } } }),
            json!({ "hooks": { "events": { "Stop": [{ "hooks": {} }] } } }),
        ] {
            assert!(merge_zcode(&mut broken.clone(), EXE).is_err(), "{broken}");
        }
    }

    #[test]
    fn remove_restores_the_users_layout_byte_for_byte() {
        let original = "{\n  \"plugins\": {\n    \"enabledPlugins\": {\n      \"a\": true\n    }\n  }\n}\n";
        let mut root: Value = serde_json::from_str(original).expect("should parse");
        merge_zcode(&mut root, EXE).expect("merge");
        assert!(remove_zcode(&mut root));
        let mut text = serde_json::to_string_pretty(&root).expect("should serialize");
        text.push('\n');
        assert_eq!(text, original);
        assert!(!remove_zcode(&mut root), "a second remove changes nothing");
    }

    #[test]
    fn remove_keeps_the_users_own_hooks_and_switch() {
        let mut root = json!({
            "hooks": {
                "enabled": true,
                "timeoutMs": 5000,
                "events": {
                    "Stop": [{ "hooks": [
                        { "type": "command", "command": "echo own" },
                        desired_hook(EXE)
                    ] }],
                    "SessionStart": [
                        { "matcher": "startup", "hooks": [{ "type": "command", "command": "echo start" }] }
                    ]
                }
            }
        });
        assert!(remove_zcode(&mut root));
        assert_eq!(root["hooks"]["timeoutMs"], json!(5000));
        assert_eq!(root["hooks"]["enabled"], Value::Bool(true), "a pre-existing switch stays");
        assert_eq!(event_hooks(&root, "Stop")[0]["command"], "echo own");
        assert!(root["hooks"]["events"].get("SessionStart").is_some());
    }

    #[test]
    fn remove_drops_the_empty_group_it_created() {
        let mut root = json!({ "hooks": { "events": {
            "Stop": [{ "matcher": "x", "hooks": [{ "type": "command", "command": "echo own" }] }]
        } } });
        merge_zcode(&mut root, EXE).expect("merge");
        assert_eq!(root["hooks"]["events"]["Stop"].as_array().expect("groups").len(), 2);
        assert!(remove_zcode(&mut root));
        assert_eq!(
            root["hooks"]["events"]["Stop"].as_array().expect("groups").len(),
            1,
            "only the user's matched group remains"
        );
    }

    fn temp_config(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kurocat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("config.json")
    }

    #[test]
    fn install_writes_once_and_uninstall_restores_the_original() {
        let path = temp_config("round-trip");
        let original = "{\n  \"plugins\": {\n    \"enabledPlugins\": {\n      \"a\": true\n    }\n  }\n}\n";
        std::fs::write(&path, original).expect("seed config");

        assert_eq!(install_zcode(&path, EXE), Ok(FileChange::Written));
        assert_eq!(install_zcode(&path, EXE), Ok(FileChange::Unchanged), "no rewrite while current");
        assert!(state_at(&path, EXE).installed);
        assert!(!state_at(&path, STALE_EXE).installed, "another executable is reported as stale");
        assert_eq!(
            install_zcode(&path, STALE_EXE),
            Ok(FileChange::Written),
            "a stale path is repaired"
        );

        assert_eq!(uninstall_zcode(&path), Ok(FileChange::Written));
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), original);
        assert_eq!(uninstall_zcode(&path), Ok(FileChange::Unchanged));
        assert!(!state_at(&path, EXE).installed);
        std::fs::remove_dir_all(path.parent().expect("temp dir")).expect("cleanup");
    }

    #[test]
    fn uninstall_leaves_foreign_hooks_alone() {
        let path = temp_config("foreign");
        let foreign = "{\n  \"hooks\": {\n    \"enabled\": true,\n    \"events\": {\n      \"Stop\": [\n        {\n          \"hooks\": [\n            {\n              \"type\": \"command\",\n              \"command\": \"echo own\"\n            }\n          ]\n        }\n      ]\n    }\n  }\n}\n";
        std::fs::write(&path, foreign).expect("seed config");
        assert_eq!(uninstall_zcode(&path), Ok(FileChange::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), foreign);
        std::fs::remove_dir_all(path.parent().expect("temp dir")).expect("cleanup");
    }

    #[test]
    fn install_creates_a_missing_config_private_and_empties_it_on_uninstall() {
        let path = temp_config("create");
        assert_eq!(install_zcode(&path, EXE), Ok(FileChange::Written));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("json");
        assert_eq!(written["hooks"]["enabled"], Value::Bool(true));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600,
                "a config we create is private, like the agent's own"
            );
        }
        assert_eq!(uninstall_zcode(&path), Ok(FileChange::Written));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "{}\n",
            "a config we created is emptied, never deleted"
        );
        std::fs::remove_dir_all(path.parent().expect("temp dir")).expect("cleanup");
    }

    #[test]
    fn a_broken_config_is_reported_and_never_rewritten() {
        let path = temp_config("broken");
        std::fs::write(&path, "{ not json").expect("seed config");
        assert!(install_zcode(&path, EXE).is_err());
        assert!(uninstall_zcode(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "{ not json");
        let state = state_at(&path, EXE);
        assert!(!state.installed);
        assert!(state.error.is_some());
        std::fs::remove_dir_all(path.parent().expect("temp dir")).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn install_keeps_the_original_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_config("permissions");
        std::fs::write(&path, "{\n  \"plugins\": {}\n}\n").expect("seed config");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        assert_eq!(install_zcode(&path, EXE), Ok(FileChange::Written));
        assert_eq!(std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(path.parent().expect("temp dir")).expect("cleanup");
    }
}

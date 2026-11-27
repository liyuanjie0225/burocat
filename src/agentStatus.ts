import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/**
 * AI agent task status (ZCode, …): the backend reports turn lifecycle events as
 * `kuro-agent-status` snapshots. This module turns them into a small "working" pill above
 * the cat and into one-shot notices (finished / waiting for the user).
 *
 * Mirrors `src-tauri/src/agent_status.rs`.
 */

/**
 * A turn shows in the pill this long after its last report. Agents only report between
 * tool calls, so a quiet turn may be a single long command — or one the user interrupted,
 * which agents never report at all. The backend keeps quiet turns for much longer, so a
 * late end is still announced with its title and duration.
 */
export const AGENT_ACTIVE_MS = 3 * 60 * 1000;

/**
 * Fallback for agents whose config lacks our hooks (auto-install off or install
 * failed): there a real approval card can never report itself, so a turn that goes
 * silent this long is probably blocked on the user — shown as a hedged "probably
 * waiting for you". With our hooks installed a real card always fires PermissionRequest
 * (verified 2026-11-26 16:50: build-mode Write cards, 2/2 cards → 2/2 events with full
 * payload), so no guessing happens at all then — a quiet turn is the model thinking or
 * one long command, and guessing misread exactly that: a guarded turn reasoning past
 * 20s before its first tool showed "等确认" with no card anywhere (decision D9). The
 * backend pushes install state on `kuro-agent-hooks`; it seeds this module.
 */
export const AGENT_SILENCE_WAITING_MS = 45 * 1000;

/** Without hooks, guarded-mode turns pause for approval on every change, so their
 * silences are usually a card on screen rather than a long think — the hint comes
 * sooner for them. Never reached while our hooks are installed: no guessing at all. */
export const AGENT_GUARDED_SILENCE_WAITING_MS = 20 * 1000;

/** Mirrors the backend's `HookState` (`agent_hooks.rs`), pushed on `kuro-agent-hooks`. */
export interface AgentHookInfo {
  detected: boolean;
  installed: boolean;
  configPath: string;
  command: string;
  events: string[];
  error: string | null;
}

/**
 * Whether our hook entries are in the agent's config and point at the running app. While
 * they are, a real approval card reports itself via PermissionRequest, so silence is
 * never guessed into a wait — only real `waiting` events show. Seeded from
 * `get_agent_hook_info`; kept fresh by `kuro-agent-hooks`.
 */
let hooksInstalled = false;

/** Quiet longer than this and the turn is presumed gone (crash without a Stop). */
const AGENT_STALE_MS = 10 * 60 * 1000;

/**
 * A running turn quiet this long reads as the model thinking: the cat pauses its
 * typing and gazes curious instead. A turn with a started-but-unfinished tool is
 * never thinking — a long command is busy work, not deliberation.
 */
export const AGENT_THINKING_MS = 6 * 1000;

const TICK_MS = 1000;
/** Longer titles are clipped so the duration stays visible in the narrow toast. */
const NOTICE_LABEL_MAX = 10;
/** Below this window width (small scales) the toast only has room for the essentials. */
const COMPACT_WIDTH = 340;
/** Space between the pill and the toast it sits underneath. */
const NOTICE_GAP = 6;
/** Input sooner than this after a prompt is not taken as the answer to it. */
const ANSWER_GRACE_MS = 1000;

export interface AgentTask {
  id: string;
  source: string;
  label: string;
  model: string;
  waiting: boolean;
  /** The tool a permission prompt is about, while waiting. */
  tool: string;
  /** Tool calls finished so far in the turn. */
  tools: number;
  /** The tool the turn ran most recently (embodiment tool bubbles). */
  lastTool: string;
  /** The tool started but not finished yet: empty when the model is on its own. */
  pendingTool: string;
  /** The agent's permission mode (`guarded` pauses for approvals on changes). */
  permissionMode: string;
  startedMs: number;
  lastEventMs: number;
}

export interface AgentDone {
  id: string;
  source: string;
  label: string;
  model: string;
  elapsedMs: number;
  interrupted: boolean;
}

export interface AgentSnapshot {
  tasks: AgentTask[] | null;
  running: number;
  waiting: number;
  lastDone: AgentDone | null;
  seq: number;
}

const SOURCE_LABELS: Record<string, string> = { zcode: 'ZCode', cursor: 'Cursor' };

export function agentSourceLabel(source: string): string {
  return SOURCE_LABELS[source] ?? source;
}

/** `account:bigmodel/GLM-5.4` → `GLM-5.4`: the last segment of whatever the agent calls it. */
export function agentModelLabel(model: string): string {
  return model.slice(model.lastIndexOf('/') + 1).trim();
}

export function formatAgentDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(total / 60);
  if (minutes >= 60) return `${Math.floor(minutes / 60)}h${minutes % 60}m`;
  return minutes > 0 ? `${minutes}m${total % 60}s` : `${total}s`;
}

/** Turns that count as active right now; the backend lists waiting ones first. Waiting
 * turns stay visible however long the user takes, quiet ones fade out only when stale. */
export function activeAgentTasks(snapshot: AgentSnapshot | null, now: number): AgentTask[] {
  return (snapshot?.tasks ?? []).filter((task) => task.waiting || now - task.lastEventMs < AGENT_STALE_MS);
}

/** Permission modes where an approval card can ever appear. ZCode's `yolo` (自主模式)
 * never asks, so its silences can never be a card — they are long tools or thinking. */
export function agentModeHasCards(permissionMode: string): boolean {
  return permissionMode !== 'yolo';
}

/** A running turn that went silent: only where a real card could NOT report itself —
 * our hooks missing from the agent's config — does silence suggest a hidden card.
 * Guarded turns pause for approval on every change, so their threshold is shorter. */
export function agentTaskProbablyWaiting(task: AgentTask, now: number): boolean {
  if (hooksInstalled) return false;
  if (!agentModeHasCards(task.permissionMode)) return false;
  const threshold =
    task.permissionMode === 'guarded' ? AGENT_GUARDED_SILENCE_WAITING_MS : AGENT_SILENCE_WAITING_MS;
  return !task.waiting && now - task.lastEventMs >= threshold;
}

/**
 * A running turn whose model is between tools: quiet for a while, nothing pending.
 * The same predicate drives the pill icon and the embodiment acting, so the two
 * never disagree about what the cat is doing.
 */
export function agentTaskThinking(task: AgentTask, now: number): boolean {
  return !task.waiting && !task.pendingTool && now - task.lastEventMs >= AGENT_THINKING_MS;
}

/** Text for the pill above the cat, or `null` when nothing is running. */
export function agentPillText(tasks: AgentTask[], now: number, width: number): string | null {
  if (tasks.length === 0) return null;
  const narrow = width < 260;
  const certain = tasks.filter((task) => task.waiting);
  const probable = tasks.filter((task) => agentTaskProbablyWaiting(task, now));
  if (certain.length > 0 || probable.length > 0) {
    // Waiting is the pill's own state (no toast duplicates it); the ticking wait time
    // doubles as a heartbeat, so a stuck wait never looks like a frozen label.
    const waited = formatAgentDuration(now - (certain[0] ?? probable[0]).lastEventMs);
    if (narrow) return `⏸ 等确认 ${waited}`;
    const verb = certain.length > 0 ? '等你确认' : '可能在等你确认';
    const running = tasks.length - certain.length - probable.length;
    const lead = agentSourceLabel((certain[0] ?? probable[0]).source);
    return `⏸ ${lead} ${verb} · ${waited}${running > 0 ? ` · ${running} 个进行中` : ''}`;
  }
  // The icon follows the freshest running task, the one the cat is acting out.
  const freshest = tasks.reduce((a, b) => (b.lastEventMs >= a.lastEventMs ? b : a));
  const icon = agentTaskThinking(freshest, now) ? '💭' : '⏳';
  if (tasks.length === 1) {
    const elapsed = formatAgentDuration(now - tasks[0].startedMs);
    const model = agentModelLabel(tasks[0].model);
    const steps = tasks[0].tools;
    if (narrow) return [icon, model, steps ? `${steps}步` : '', elapsed].filter(Boolean).join(' ');
    const progress = [
      agentSourceLabel(tasks[0].source),
      model,
      steps ? `${steps} 步` : '',
    ].filter(Boolean);
    return `${icon} ${progress.join(' · ')} · ${elapsed}`;
  }
  return narrow ? `${icon} ${tasks.length} 个` : `${icon} ${tasks.length} 个任务进行中`;
}

/** One-shot notice for a turn that just ended. Narrow windows drop the title, so the
 *  agent, the outcome and the duration always stay visible. */
export function agentDoneText(done: AgentDone, compact = false): string {
  const elapsed = done.elapsedMs >= 2000 ? ` · ${formatAgentDuration(done.elapsedMs)}` : '';
  const source = agentSourceLabel(done.source);
  if (done.interrupted) {
    return compact
      ? `⏹ ${source} 已中断${elapsed}`
      : `⏹ ${source}「${clipLabel(done.label)}」已中断${elapsed}`;
  }
  if (compact || !done.label) return `✅ ${source} 完成${elapsed}`;
  return `✅ ${source}「${clipLabel(done.label)}」完成${elapsed}`;
}

function clipLabel(label: string): string {
  const chars = Array.from(label);
  return chars.length > NOTICE_LABEL_MAX ? `${chars.slice(0, NOTICE_LABEL_MAX).join('')}…` : label;
}

/** task id → the `lastEventMs` of the prompt the user has answered. */
export type AnsweredPrompts = Map<string, number>;

/**
 * Agents report that a permission prompt appeared, but not that it was answered. Any key
 * or click `ANSWER_GRACE_MS` later is taken as the answer: somebody who is away never
 * touches anything. The next prompt re-arms the wait by itself (its `lastEventMs` differs).
 */
export function noteUserActivity(tasks: AgentTask[], answered: AnsweredPrompts, now: number): void {
  for (const task of tasks) {
    if (task.waiting && now - task.lastEventMs >= ANSWER_GRACE_MS) {
      answered.set(task.id, task.lastEventMs);
    }
  }
}

/** Tasks as displayed: a prompt the user answered no longer counts as waiting. */
export function shownAgentTasks(tasks: AgentTask[], answered: AnsweredPrompts): AgentTask[] {
  return tasks.map((task) =>
    task.waiting && answered.get(task.id) === task.lastEventMs ? { ...task, waiting: false } : task,
  );
}

export interface AgentStatusOptions {
  /** Whether the feature is switched on in settings. */
  isEnabled: () => boolean;
  /**
   * Bottom edge (window px) of the toast above the head, or `null` while it is hidden.
   * The pill shares that headroom and sits right underneath rather than disappearing:
   * an update toast can stay for good.
   */
  noticeBottom: () => number | null;
  /** One-shot announcements (a turn finished or was interrupted). Waiting never
   * toasts — the pill owns that state, the cat looking up is the ping. */
  showNotice: (text: string) => void;
  /** Small attention cue for the cat (a turn ended, or one is waiting for the user). */
  onAttention: () => void;
}

export async function initAgentStatus(options: AgentStatusOptions): Promise<void> {
  const pill = document.getElementById('agent-pill');
  if (!pill) return;

  let snapshot: AgentSnapshot | null = null;
  let wasWaiting = false;
  const answered: AnsweredPrompts = new Map();

  // Tell the backend what is on screen (it only knows what it emitted), so the local
  // diagnostics endpoint can tell "event accepted" from "event shown". On change only.
  let reported = '';
  const report = (notice?: string): void => {
    const shown = !pill.classList.contains('hidden');
    const waiting = pill.classList.contains('waiting');
    const state = `${shown ? pill.textContent : ''}|${waiting}`;
    if (notice === undefined && state === reported) return;
    reported = state;
    void invoke('agent_ui_reported', {
      pill: shown ? pill.textContent : null,
      waiting,
      notice: notice ?? null,
    });
  };

  const render = (): void => {
    const now = Date.now();
    const tasks = shownAgentTasks(activeAgentTasks(snapshot, now), answered);
    const text = options.isEnabled() ? agentPillText(tasks, now, window.innerWidth) : null;
    // Waiting can arrive two ways: reported by a permission event, or derived here when
    // a turn goes silent while our hooks are absent (with them a real card always
    // reports itself). The rising edge is detected in this render — not
    // only in the event listener — so silence announces itself too. No toast: the pill
    // already says "waiting" persistently, and a toast would linger after the user
    // answers, arguing with the pill; the cat looking up is the ping.
    const waitingNow = tasks.some((task) => task.waiting || agentTaskProbablyWaiting(task, now));
    if (waitingNow && !wasWaiting && options.isEnabled()) {
      options.onAttention();
    }
    wasWaiting = waitingNow;
    const below = options.noticeBottom();
    const gap = window.innerWidth < 260 ? 4 : NOTICE_GAP;
    pill.textContent = text ?? '';
    pill.classList.toggle('hidden', text === null);
    pill.classList.toggle('waiting', waitingNow);
    pill.style.top = text !== null && below !== null ? `${Math.round(below + gap)}px` : '';
    report();
  };

  // Whether real cards report themselves decides if silence may be guessed as waiting;
  // the backend re-pushes on every install change (settings toggle, exe switch).
  const hooks = await invoke<AgentHookInfo>('get_agent_hook_info').catch(() => null);
  hooksInstalled = hooks?.installed ?? false;
  await listen<AgentHookInfo>('kuro-agent-hooks', (ev) => {
    if (hooksInstalled === ev.payload.installed) return;
    hooksInstalled = ev.payload.installed;
    render();
  });

  await listen<AgentSnapshot>('kuro-agent-status', (ev) => {
    if (!options.isEnabled()) return;
    snapshot = ev.payload;
    if (ev.payload.lastDone) {
      const notice = agentDoneText(ev.payload.lastDone, window.innerWidth < COMPACT_WIDTH);
      options.showNotice(notice);
      options.onAttention();
      report(notice);
    }
    render();
  });

  await listen<{ kind?: string }>('kuro-input', (ev) => {
    const kind = ev.payload?.kind;
    if (kind !== 'keyDown' && kind !== 'mouseDown' && kind !== 'privacyTyping') return;
    noteUserActivity(activeAgentTasks(snapshot, Date.now()), answered, Date.now());
    render();
  });

  // Pick up whatever is already running (e.g. after a page reload) without re-announcing it.
  snapshot = await invoke<AgentSnapshot>('get_agent_snapshot').catch(() => null);
  wasWaiting = activeAgentTasks(snapshot, Date.now()).some((task) => task.waiting);

  // The pill shows a running clock, and has to move whenever a toast comes or goes.
  window.setInterval(render, TICK_MS);
  window.addEventListener('resize', render);
  const toast = document.getElementById('update-toast');
  if (toast) {
    new MutationObserver(render).observe(toast, { attributes: true, attributeFilter: ['class'] });
  }
  render();
}

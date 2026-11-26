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

/** Turns that count as active right now; the backend lists waiting ones first. */
export function activeAgentTasks(snapshot: AgentSnapshot | null, now: number): AgentTask[] {
  return (snapshot?.tasks ?? []).filter((task) => now - task.lastEventMs < AGENT_ACTIVE_MS);
}

/** Text for the pill above the cat, or `null` when nothing is running. */
export function agentPillText(tasks: AgentTask[], now: number, width: number): string | null {
  if (tasks.length === 0) return null;
  const narrow = width < 260;
  const waiting = tasks.filter((task) => task.waiting);
  if (waiting.length > 0) {
    if (narrow) return '⏸ 等确认';
    const running = tasks.length - waiting.length;
    return `⏸ ${agentSourceLabel(waiting[0].source)} 等你确认${running > 0 ? ` · ${running} 个进行中` : ''}`;
  }
  if (tasks.length === 1) {
    const elapsed = formatAgentDuration(now - tasks[0].startedMs);
    const model = agentModelLabel(tasks[0].model);
    const steps = tasks[0].tools;
    if (narrow) return ['⏳', model, steps ? `${steps}步` : '', elapsed].filter(Boolean).join(' ');
    const progress = [
      agentSourceLabel(tasks[0].source),
      model,
      steps ? `${steps} 步` : '',
    ].filter(Boolean);
    return `⏳ ${progress.join(' · ')} · ${elapsed}`;
  }
  return narrow ? `⏳ ${tasks.length} 个` : `⏳ ${tasks.length} 个任务进行中`;
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

/** Notice for a turn that just blocked on the user; `null` when none is waiting. */
export function agentWaitingText(tasks: AgentTask[], compact = false): string | null {
  let waiting: AgentTask | null = null;
  for (const task of tasks) {
    if (task.waiting) {
      waiting = task;
      break;
    }
  }
  if (!waiting) return null;
  const source = agentSourceLabel(waiting.source);
  if (compact) return `⏸ ${source} 等确认`;
  return `⏸ ${source} 等你确认${waiting.tool ? ` ${waiting.tool}` : ''}`;
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
    const tasks = shownAgentTasks(activeAgentTasks(snapshot, Date.now()), answered);
    const text = options.isEnabled()
      ? agentPillText(tasks, Date.now(), window.innerWidth)
      : null;
    const below = options.noticeBottom();
    const gap = window.innerWidth < 260 ? 4 : NOTICE_GAP;
    pill.textContent = text ?? '';
    pill.classList.toggle('hidden', text === null);
    pill.classList.toggle('waiting', tasks.some((task) => task.waiting));
    pill.style.top = text !== null && below !== null ? `${Math.round(below + gap)}px` : '';
    report();
  };

  await listen<AgentSnapshot>('kuro-agent-status', (ev) => {
    if (!options.isEnabled()) return;
    snapshot = ev.payload;
    const compact = window.innerWidth < COMPACT_WIDTH;
    const tasks = activeAgentTasks(snapshot, Date.now());
    const waitingNow = tasks.some((task) => task.waiting);
    let notice: string | undefined;
    if (ev.payload.lastDone) {
      notice = agentDoneText(ev.payload.lastDone, compact);
      options.onAttention();
    } else if (waitingNow && !wasWaiting) {
      notice = agentWaitingText(tasks, compact) ?? undefined;
      if (notice) options.onAttention();
    }
    wasWaiting = waitingNow;
    if (notice) options.showNotice(notice);
    render();
    report(notice);
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

import { listen } from '@tauri-apps/api/event';
import { AgentSnapshot, activeAgentTasks } from './agentStatus';

/**
 * Agent embodiment state machine: turns `kuro-agent-status` snapshots into the
 * cat's performance — working (fake-typing + tool bubbles), waiting (looks up at
 * the user), done (confetti / sigh), quiet (yawn). `agentStatus.ts` keeps owning
 * the pill and notices; this module only drives the acting.
 *
 * Arbitration (PRD 3.2): password privacy > the user's real input > agent acting.
 * One-shot performances (done/yawn) are skipped while the user is typing so the
 * cat never competes with the human; the pill/notice still show, as usual.
 */

/** The performance API both render engines expose (2D `KuroCatEngine`, 3D `KuroCatEngine3D`). */
export interface EmbodimentEngine {
  setAgentWorking(active: boolean): void;
  setAgentToolLabel(label: string): void;
  setAgentWaiting(active: boolean, tool?: string): void;
  triggerAgentDone(interrupted: boolean): void;
  triggerAgentYawn(): void;
}

export interface EmbodimentOptions {
  isEnabled(): boolean;
  isPrivacyActive(): boolean;
  /** Engines to drive: persistent states go to both, one-shots only to the primary. */
  getEngines(): { primary: EmbodimentEngine | null; secondary: EmbodimentEngine | null };
}

/** A quiet turn this long gets a yawn: one long command, exactly the case agents never report. */
export const AGENT_QUIET_MS = 90 * 1000;
/** Re-derive the phase this often; keeps silence/yawn detection alive between events. */
const TICK_MS = 1000;
/** Input sooner than this after a prompt is not taken as the answer to it (same as agentStatus.ts). */
const ANSWER_GRACE_MS = 1000;
/** The user typed this recently: skip one-shot performances (user real input wins). */
const USER_PRIORITY_MS = 650;

export function initEmbodiment(options: EmbodimentOptions): void {
  let snapshot: AgentSnapshot | null = null;
  let enabled = options.isEnabled();
  let privacy = options.isPrivacyActive();
  let lastRealInputAt = -Infinity;
  /** Task ids the local answered-heuristic believes were answered, `id -> lastEventMs` of the prompt. */
  const answered = new Map<string, number>();
  /** seq of the snapshot whose `lastDone` was already performed. */
  let performedDoneSeq = -1;
  /** Tasks that already yawned in the current quiet stretch, `id -> lastEventMs` at the yawn. */
  const yawned = new Map<string, number>();
  let working = false;
  let waiting = false;
  let toolLabel = '';

  const push = (): void => {
    const { primary, secondary } = options.getEngines();
    // Persistent states go to both engines so switching 2D <-> 3D keeps the show.
    for (const engine of [primary, secondary]) {
      if (!engine) continue;
      engine.setAgentWorking(working);
      engine.setAgentToolLabel(toolLabel);
      engine.setAgentWaiting(waiting, '');
    }
  };

  const stop = (): void => {
    working = false;
    waiting = false;
    toolLabel = '';
    push();
  };

  const render = (now: number): void => {
    const nextEnabled = options.isEnabled();
    const nextPrivacy = options.isPrivacyActive();
    if (nextEnabled !== enabled || nextPrivacy !== privacy) {
      enabled = nextEnabled;
      privacy = nextPrivacy;
      if (!enabled || privacy) stop();
    }
    if (!enabled || privacy || !snapshot) return;

    const active = activeAgentTasks(snapshot, now);
    // Waiting tasks the user already answered: the agent never reports it, so
    // any keypress after a one-second grace reads as the answer (same heuristic
    // as agentStatus.ts; a new prompt re-arms because lastEventMs changes).
    const shown = active.filter((task) => !(task.waiting && answered.get(task.id) === task.lastEventMs));
    if (shown.length === 0) {
      if (working || waiting) stop();
      return;
    }
    // Perform the freshest task; the pill keeps listing all of them.
    const star = shown.reduce((a, b) => (b.lastEventMs >= a.lastEventMs ? b : a));

    working = !star.waiting;
    toolLabel = star.waiting ? '' : star.lastTool;
    waiting = star.waiting;
    push();

    if (star.waiting) return;

    // One long silent command: a yawn, once per quiet stretch. A turn quiet past
    // AGENT_ACTIVE_MS fades out of `active` on its own — the cat rests until the
    // next tool event brings it back to the keyboard.
    const quietMs = now - star.lastEventMs;
    const userTyping = now - lastRealInputAt < USER_PRIORITY_MS;
    if (quietMs >= AGENT_QUIET_MS && yawned.get(star.id) !== star.lastEventMs && !userTyping) {
      yawned.set(star.id, star.lastEventMs);
      options.getEngines().primary?.triggerAgentYawn();
    } else if (quietMs < AGENT_QUIET_MS) {
      yawned.delete(star.id);
    }
  };

  void listen<AgentSnapshot>('kuro-agent-status', (ev) => {
    snapshot = ev.payload;
    if (!snapshot) return;
    if (snapshot.lastDone && snapshot.seq !== performedDoneSeq) {
      performedDoneSeq = snapshot.seq;
      yawned.delete(snapshot.lastDone.id);
      const userTyping = performance.now() - lastRealInputAt < USER_PRIORITY_MS;
      if (enabled && !privacy && !userTyping) {
        options.getEngines().primary?.triggerAgentDone(snapshot.lastDone.interrupted);
      }
    }
    render(performance.now());
  });

  void listen<{ kind: string }>('kuro-input', (ev) => {
    const kind = ev.payload?.kind;
    if (kind !== 'keyDown' && kind !== 'mouseDown' && kind !== 'privacyTyping') return;
    const now = performance.now();
    lastRealInputAt = now;
    if (!snapshot) return;
    for (const task of snapshot.tasks ?? []) {
      if (task.waiting && now - task.lastEventMs >= ANSWER_GRACE_MS) {
        answered.set(task.id, task.lastEventMs);
      }
    }
    // An answered prompt means the cat goes back to work on the next render.
    render(now);
  });

  window.setInterval(() => render(performance.now()), TICK_MS);
  stop();
}

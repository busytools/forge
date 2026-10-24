/**
 * The conversation's task join: what its frames say about background tasks.
 *
 * **One reader for both pages that need it** - the strip's outcomes and the
 * chat's stops - so the two cannot drift on how a launch is linked, what
 * counts as a stop's success, or which frames belong to the session at all.
 *
 * **Dispatched frames are not the session's**, the same rule the chat's fold
 * keeps: a sub-agent's task and its call belong to the dispatch's timeline,
 * and a join that read them would settle a call the session never draws.
 */

import { blocksOf, launchedTask, stopSucceeded } from './leaves';

/** What a task frame says ended a call, before the page marks it stopped. */
export interface TaskEnding {
  failed: boolean;
  ended_ms: number | null;
}

/** Everything one walk of a page's frames says about its tasks. */
export interface TaskJoin {
  /** The call a task belongs to: `task_started` first, a launch sentence
   * accepted only where the CLI's own link has not spoken. */
  owners: Map<string, string>;
  /** The tasks `task_started` linked itself: the CLI's own word, which wins
   * over a launch sentence wherever either sits on the page. */
  strong: Set<string>;
  /** The calls whose `tool_use` the page holds. */
  held: Set<string>;
  /** The `TaskStop` calls the page holds, by the task each names. */
  asks: Map<string, string>;
  /** Whether each stop's own answer reported success, by the stop's call. */
  answers: Map<string, boolean>;
  /** The endings the task frames stated, by the call each ended. */
  calls: Map<string, TaskEnding>;
}

/** A page nothing has been read from yet. */
export function seedJoin(): TaskJoin {
  return {
    owners: new Map(),
    strong: new Set(),
    held: new Set(),
    asks: new Map(),
    answers: new Map(),
    calls: new Map(),
  };
}

/** Whether a frame came from a dispatched agent rather than the session's own. */
export function isDispatched(frame: unknown): boolean {
  const held = frame as { parent_tool_use_id?: unknown } | null;
  // The wire spells "no dispatch" as null and a dispatch's own tool-use id
  // otherwise, which is the same field and the same guard the server's fold
  // reads. The sidechain flag a transcript FILE carries is not on the wire at
  // all: `Message` has no such field, and no captured baseline contains the
  // string - so a guard keyed on it would read as working and suppress
  // nothing, which is worse than no guard.
  return typeof held?.parent_tool_use_id === 'string' && held.parent_tool_use_id.trim() !== '';
}

interface Frame {
  type?: unknown;
  subtype?: unknown;
  task_id?: unknown;
  tool_use_id?: unknown;
  status?: unknown;
  patch?: unknown;
  message?: { content?: unknown };
}

/** Fold one frame into the join. */
export function addJoinFrame(join: TaskJoin, frame: unknown): void {
  if (isDispatched(frame)) return;
  const held = frame as Frame | null;
  if (held === null || typeof held !== 'object') return;
  if (held.type === 'assistant' || held.type === 'user') {
    for (const block of blocksOf(held.message?.content)) {
      if (block.type === 'tool_use' && typeof block.id === 'string') {
        join.held.add(block.id);
        if (block.name === 'TaskStop') {
          const task = (block.input as { task_id?: unknown } | null)?.task_id;
          if (typeof task === 'string') join.asks.set(block.id, task);
        }
      }
      if (block.type === 'tool_result' && typeof block.tool_use_id === 'string') {
        const task = launchedTask(block.content);
        // **First write wins.** Any result may quote the sentence (a `Read` of
        // a log, a `Bash` echoing one), and a later copy must not steal the
        // task from the call that launched it.
        if (task !== null && !join.owners.has(task)) join.owners.set(task, block.tool_use_id);
        if (join.asks.has(block.tool_use_id)) {
          join.answers.set(block.tool_use_id, stopSucceeded(block));
        }
      }
    }
    return;
  }
  if (held.type !== 'system') return;
  const task = typeof held.task_id === 'string' ? held.task_id : null;
  if (held.subtype === 'task_started') {
    const call = held.tool_use_id;
    if (task === null || typeof call !== 'string') return;
    join.owners.set(task, call);
    join.strong.add(task);
    return;
  }
  // task_updated names only the task; task_notification names the call.
  const direct = typeof held.tool_use_id === 'string' ? held.tool_use_id : null;
  const call = direct ?? (task === null ? null : (join.owners.get(task) ?? null));
  if (call === null) return;
  const patch =
    typeof held.patch === 'object' && held.patch !== null
      ? (held.patch as { status?: unknown; end_time?: unknown })
      : null;
  const status =
    typeof patch?.status === 'string'
      ? patch.status
      : typeof held.status === 'string'
        ? held.status
        : null;
  const ended = typeof patch?.end_time === 'number' ? patch.end_time : null;
  const prior = join.calls.get(call);
  join.calls.set(call, {
    // Latched: a later frame cannot un-fail a call.
    failed: (prior?.failed ?? false) || status === 'failed',
    ended_ms: ended ?? prior?.ended_ms ?? null,
  });
}

/** One stop that succeeded, and what the page can say about its target. */
export interface StoppedTask {
  /** The `TaskStop` call whose own answer said it stopped. */
  stop: string;
  /** The task id the stop named. */
  task: string;
  /** The call the task was launched from, when the page links them. */
  call: string | null;
}

/** Every stop the join's own answers report, in the order the asks were read. */
export function settleStops(join: TaskJoin): StoppedTask[] {
  const out: StoppedTask[] = [];
  for (const [stop, task] of join.asks) {
    if (join.answers.get(stop) !== true) continue;
    out.push({ stop, task, call: join.owners.get(task) ?? null });
  }
  return out;
}

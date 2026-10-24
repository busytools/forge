/**
 * A call's outcome, read from the conversation's own task frames.
 *
 * The CLI reports a task's end in two frames: `task_updated` carries
 * `patch{status, end_time}` but names only the task id, while
 * `task_notification` carries the status and the tool_use_id directly. Both
 * are folded here by tool_use_id, so a settled row in the strip can say
 * whether its call failed and when it ended - the same frames the chat's own
 * row reads, joined at the same ids.
 *
 * **A `TaskStop` carries an ending of its own, and no frame follows it.** The
 * tool path emits no `task_started` and no task frame at all (measured:
 * session 4b35ed0e), so the sentence the launch result wrote is the only link
 * from the task to its call, and the tool's success answer is the only ending.
 */

import { blocksOf, launchedTask, stopSucceeded, type Block } from './leaves';

/** What is known about a finished call, by its tool_use_id. */
export interface Outcome {
  /** The CLI's own verdict: only `failed` counts, so an unknown word cannot
   * paint a healthy call as broken. */
  failed: boolean;
  /** The end time the frame carried, unix ms; null when none arrived. */
  ended_ms: number | null;
  /** Whether a `TaskStop` ended the call. A stopped task is not running even
   * while the CLI's registry still lists it, because the tool path sends no
   * registry change either. */
  stopped: boolean;
}

/** The conversation's task facts, in the two shapes a row needs them. */
export interface Outcomes {
  /** Verdicts and end times, by the call each names. */
  calls: Map<string, Outcome>;
  /** The call a task belongs to, from `task_started` or a launch result's own
   * sentence - the registry row the wire sends often predates the link (the
   * CLI announces the task, then names the call), so a row reads its id from
   * here when its own is null. */
  owners: Map<string, string>;
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

export function outcomesFrom(turns: readonly { messages: readonly unknown[] }[]): Outcomes {
  /** The call a task belongs to, from `task_started` and launch results. */
  const owners = new Map<string, string>();
  const out = new Map<string, Outcome>();
  /** The result each call answered with, by the call it ended. */
  const results = new Map<string, Block>();
  /** The `TaskStop` calls the conversation holds, by the task each names. */
  const stopAsks: Array<{ call: string; task: string }> = [];

  for (const turn of turns) {
    for (const message of turn.messages) {
      const frame = message as Frame | null;
      if (frame === null) continue;
      if (frame.type === 'assistant' || frame.type === 'user') {
        for (const block of blocksOf(frame.message?.content)) {
          if (block.type === 'tool_use' && typeof block.id === 'string') {
            const task = (block.input as { task_id?: unknown } | null)?.task_id;
            if (block.name === 'TaskStop' && typeof task === 'string') {
              stopAsks.push({ call: block.id, task });
            }
          }
          if (block.type === 'tool_result' && typeof block.tool_use_id === 'string') {
            results.set(block.tool_use_id, block);
            const task = launchedTask(block.content);
            // `task_started` is the CLI's own link and wins where both exist.
            if (task !== null && !owners.has(task)) owners.set(task, block.tool_use_id);
          }
        }
        continue;
      }
      if (frame.type !== 'system') continue;
      const task = typeof frame.task_id === 'string' ? frame.task_id : null;
      if (frame.subtype === 'task_started') {
        const call = frame.tool_use_id;
        if (task !== null && typeof call === 'string') owners.set(task, call);
        continue;
      }
      // task_updated names only the task; task_notification names the call.
      const direct = typeof frame.tool_use_id === 'string' ? frame.tool_use_id : null;
      const call = direct ?? (task === null ? null : (owners.get(task) ?? null));
      if (call === null) continue;
      const patch =
        typeof frame.patch === 'object' && frame.patch !== null
          ? (frame.patch as { status?: unknown; end_time?: unknown })
          : null;
      const status =
        typeof patch?.status === 'string'
          ? patch.status
          : typeof frame.status === 'string'
            ? frame.status
            : null;
      const ended = typeof patch?.end_time === 'number' ? patch.end_time : null;
      const prior = out.get(call);
      out.set(call, {
        // Latched: a later frame cannot un-fail a call.
        failed: (prior?.failed ?? false) || status === 'failed',
        ended_ms: ended ?? prior?.ended_ms ?? null,
        stopped: prior?.stopped === true,
      });
    }
  }

  // **A stop's own answer ends the call it names**, where the conversation
  // holds both halves: the tool reports success, the launch result named the
  // task, and the link between them is what says which call stopped.
  for (const ask of stopAsks) {
    if (!stopSucceeded(results.get(ask.call))) continue;
    const call = owners.get(ask.task);
    if (call === undefined) continue;
    const prior = out.get(call);
    out.set(call, {
      failed: prior?.failed ?? false,
      ended_ms: prior?.ended_ms ?? null,
      stopped: true,
    });
  }
  return { calls: out, owners };
}

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
 * **A `TaskStop` carries an ending of its own, and no frame follows it**
 * (4b35ed0e): the tool path emits no `task_started` and no task frame at all,
 * so the sentence the launch result wrote is the only link from the task to
 * its call, and the tool's success answer is the only ending.
 */

import { addJoinFrame, seedJoin, settleStops } from './join';

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

/** The conversation's task facts, in the shapes a row needs them. */
export interface Outcomes {
  /** Verdicts and end times, by the call each names. */
  calls: Map<string, Outcome>;
  /** The call a task belongs to, from `task_started` or a launch result's own
   * sentence - the registry row the wire sends often predates the link (the
   * CLI announces the task, then names the call), so a row reads its id from
   * here when its own is null. */
  owners: Map<string, string>;
  /**
   * The tasks a `TaskStop` ended, by the task id the stop itself names.
   *
   * **The stop's own id, because the launch can be out of the page's window.**
   * The record holds the newest turns (`SUBSCRIBE_TURNS`), and a task started
   * earlier in the session - a dev server, a long gate - has its launch result
   * outside it, so `owners` cannot resolve the call. The registry row carries
   * the task id, though, and that is enough to stop counting it.
   */
  stoppedTasks: Set<string>;
}

export function outcomesFrom(turns: readonly { messages: readonly unknown[] }[]): Outcomes {
  const join = seedJoin();
  for (const turn of turns) {
    for (const message of turn.messages) addJoinFrame(join, message);
  }

  const calls = new Map<string, Outcome>();
  for (const [call, ending] of join.calls) calls.set(call, { ...ending, stopped: false });

  // **A stop's own answer ends the task it names**, wherever on the page its
  // launch sits - or whether it sits there at all. The call-level outcome is
  // the reveal link's, where the owner is known.
  const stoppedTasks = new Set<string>();
  for (const stopped of settleStops(join)) {
    stoppedTasks.add(stopped.task);
    if (stopped.call === null) continue;
    const prior = calls.get(stopped.call);
    calls.set(stopped.call, {
      failed: prior?.failed ?? false,
      ended_ms: prior?.ended_ms ?? null,
      stopped: true,
    });
  }
  return { calls, owners: join.owners, stoppedTasks };
}

/**
 * A call's outcome, read from the conversation's own task frames.
 *
 * The CLI reports a task's end in two frames: `task_updated` carries
 * `patch{status, end_time}` but names only the task id, while
 * `task_notification` carries the status and the tool_use_id directly. Both
 * are folded here by tool_use_id, so a settled row in the strip can say
 * whether its call failed and when it ended - the same frames the chat's own
 * row reads, joined at the same ids.
 */

/** What is known about a finished call, by its tool_use_id. */
export interface Outcome {
  /** The CLI's own verdict: only `failed` counts, so an unknown word cannot
   * paint a healthy call as broken. */
  failed: boolean;
  /** The end time the frame carried, unix ms; null when none arrived. */
  ended_ms: number | null;
}

/** The conversation's task facts, in the two shapes a row needs them. */
export interface Outcomes {
  /** Verdicts and end times, by the call each names. */
  calls: Map<string, Outcome>;
  /** The call a task belongs to, from `task_started` - the registry row the
   * wire sends often predates the link (the CLI announces the task, then
   * names the call), so a row reads its id from here when its own is null. */
  owners: Map<string, string>;
}

interface Frame {
  type?: unknown;
  subtype?: unknown;
  task_id?: unknown;
  tool_use_id?: unknown;
  status?: unknown;
  patch?: unknown;
}

export function outcomesFrom(turns: readonly { messages: readonly unknown[] }[]): Outcomes {
  /** The call a task belongs to, recorded where `task_started` names both. */
  const owners = new Map<string, string>();
  const out = new Map<string, Outcome>();
  for (const turn of turns) {
    for (const message of turn.messages) {
      const frame = message as Frame | null;
      if (frame === null || frame.type !== 'system') continue;
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
      });
    }
  }
  return { calls: out, owners };
}

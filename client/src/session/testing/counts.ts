/**
 * What the inspector's own builders were asked to do, in the order they were
 * asked.
 *
 * A module rather than a prop, for the reason the chat's list double needs one:
 * the counter has to live inside a `vi.mock` factory, which is hoisted above
 * the test file's own imports and cannot close over anything the test declares.
 * This module is the seam between the wrapped function and the assertion.
 *
 * **The counts are the instrument, and its control is a zero.** A run that
 * changes nothing the inspector draws must report every builder at 0, so a
 * number here that is merely a render count shows up as a control that will
 * not go quiet rather than as a plausible measurement.
 */

/** One builder call: which builder, and the rows a record it was given
 * carried. */
export interface Call {
  name: string;
  /**
   * The frames a conversation-carrying argument held, or `null` for one that is
   * not one.
   *
   * **The volume a measurement is taken against, read off the record itself.**
   * The page no longer walks the conversation it is given, so a fixture that
   * stopped carrying one would leave every number in the file green.
   */
  frames: number | null;
}

const called: Call[] = [];

export function clear(): void {
  called.length = 0;
}

export function countOf(name: string): number {
  return called.filter((call) => call.name === name).length;
}

/** The names called, with their call counts, in first-call order. */
export function tally(): { name: string; calls: number }[] {
  const order: string[] = [];
  const seen = new Map<string, number>();
  for (const call of called) {
    if (!seen.has(call.name)) order.push(call.name);
    seen.set(call.name, (seen.get(call.name) ?? 0) + 1);
  }
  return order.map((name) => ({ name, calls: seen.get(name) ?? 0 }));
}

/** Wrap one builder so every call is recorded. */
export function counting<F extends (...args: never[]) => unknown>(name: string, fn: F): F {
  const wrapped = (...args: never[]): unknown => {
    const first: unknown = args[0];
    called.push({ name, frames: carriedRows(first) });
    return fn(...args);
  };
  return wrapped as F;
}

/**
 * The rows one builder's first argument carried, or `null` for an argument
 * that is not a session record.
 *
 * A record's conversation is turns of frames, so the count is the frames
 * flattened - the same volume the page used to walk when it scanned for a
 * dispatch, read here off the record instead of off the work.
 */
function carriedRows(first: unknown): number | null {
  if (first === null || typeof first !== 'object') return null;
  const conversation = (first as { conversation?: unknown }).conversation;
  if (conversation === null || typeof conversation !== 'object') return null;
  const turns = (conversation as { turns?: unknown }).turns;
  if (!Array.isArray(turns)) return null;
  return turns.reduce((total: number, turn: unknown) => {
    const messages = (turn as { messages?: unknown } | null)?.messages;
    return total + (Array.isArray(messages) ? messages.length : 0);
  }, 0);
}

/**
 * The rows the LAST record handed to one builder carried, or 0 when it got
 * none.
 *
 * The last rather than the sum: what the assertion is about is the record the
 * page holds, and a page that re-read would otherwise double the number and
 * read as a bigger conversation rather than as a fault.
 */
export function rowsCarried(name: string): number {
  return called.filter((call) => call.name === name).at(-1)?.frames ?? 0;
}

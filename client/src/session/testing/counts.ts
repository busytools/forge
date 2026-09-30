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

/** One builder call: which builder, and the size of the array it was handed. */
export interface Call {
  name: string;
  /** The length of the first argument when it is an array, else `null`. */
  width: number | null;
}

const called: Call[] = [];

export function clear(): void {
  called.length = 0;
}

export function countOf(name: string): number {
  return called.filter((call) => call.name === name).length;
}

/**
 * The total width of what one builder was handed.
 *
 * **A call count cannot tell an empty conversation from a full one**, which is
 * the whole point of the record this suite measures against: a fixture mutated
 * to hand the page no messages at all still calls every builder exactly once.
 * So the volume has to be asserted separately from the calls, and this is it.
 */
export function widthOf(name: string): number {
  return called.reduce((total, call) => total + (call.name === name ? (call.width ?? 0) : 0), 0);
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
    called.push({ name, width: Array.isArray(first) ? first.length : null });
    return fn(...args);
  };
  return wrapped as F;
}

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

/** One builder call: which builder, and the size of what it was handed. */
export interface Call {
  name: string;
  /** The length of the first argument when it is an array, else `null`. */
  width: number | null;
}

export const calls: Call[] = [];

export function clear(): void {
  calls.length = 0;
}

/** Every recorded call to one builder. */
export function of(name: string): Call[] {
  return calls.filter((call) => call.name === name);
}

export function countOf(name: string): number {
  return of(name).length;
}

/** The names called, with their call counts, in first-call order. */
export function tally(): { name: string; calls: number }[] {
  const order: string[] = [];
  const seen = new Map<string, number>();
  for (const call of calls) {
    if (!seen.has(call.name)) order.push(call.name);
    seen.set(call.name, (seen.get(call.name) ?? 0) + 1);
  }
  return order.map((name) => ({ name, calls: seen.get(name) ?? 0 }));
}

/**
 * The total width of what one builder was handed.
 *
 * The denominator an assertion needs: a builder that ran once over nothing and
 * one that ran once over the whole conversation are the same call count, and
 * only the width tells them apart.
 */
export function widthOf(name: string): number {
  return of(name).reduce((total, call) => total + (call.width ?? 0), 0);
}

/**
 * Wrap one builder so every call is recorded.
 *
 * `width` is the argument whose length is the denominator, named per builder:
 * the dispatch scan is handed the whole conversation and its cost is that
 * length, while the section builders are handed a record and their cost is not
 * a length at all.
 */
export function counting<F extends (...args: never[]) => unknown>(name: string, fn: F): F {
  const wrapped = (...args: never[]): unknown => {
    const first: unknown = args[0];
    calls.push({ name, width: Array.isArray(first) ? first.length : null });
    return fn(...args);
  };
  return wrapped as F;
}

/**
 * The last measured height of each turn's row, by the turn's key.
 *
 * **The list's virtualiser caches sizes by key, and drops the size of a
 * removed LAST item** - and the queue hold removes exactly that row on a send
 * and re-adds it on drain, so the re-added turn was drawn at the estimator's
 * median for one frame: the up-and-down shake on every Enter (#1890, confirmed
 * live 2026-10-09). This map is the column's own memory of what each row
 * measured, and the list is seeded from it - a re-added row comes back at the
 * height it had, so the two frames of the send's own cycle land as one.
 */
const heights = new Map<string, number>();

/**
 * The size a row with no measurement is drawn at, until it is measured.
 *
 * The virtualiser's own estimate for an unmeasured row (its median, measured
 * at 386px during #1890), so seeding the list changes nothing for rows this
 * map has never seen - a new turn draws exactly as it did before this.
 */
export const ROW_ESTIMATE = 386;

/** What `key`'s row last measured, or `undefined` for one never measured. */
export function sizeOf(key: string | null | undefined): number | undefined {
  return key === null || key === undefined ? undefined : heights.get(key);
}

/** Record a row's height. A zero-size render is a layout in flight, not a fact. */
export function remember(key: string, height: number): void {
  if (height > 0) heights.set(key, height);
}

/** Forget every key the conversation no longer holds, so the map stays the seat's. */
export function prune(keys: Iterable<string>): void {
  const held = new Set(keys);
  for (const key of heights.keys()) {
    if (!held.has(key)) heights.delete(key);
  }
}

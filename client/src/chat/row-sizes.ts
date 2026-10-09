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
 *
 * **One store per column, not one per module.** Keys are the turn's own and
 * not seat-qualified, and a column prunes every key it does not hold - so a
 * shared map would let one seat's prune forget a neighbour's rows and bring the
 * shake back on the next switch. `sizes()` closes the map over each column.
 */
export const ROW_ESTIMATE = 386;

/** A column's own memory of what each of its rows measured. */
export interface RowSizes {
  /** What `key`'s row last measured, or `undefined` for one never measured. */
  sizeOf(key: string | null | undefined): number | undefined;
  /** Record a row's height. A zero-size render is a layout in flight, not a fact. */
  remember(key: string, height: number): void;
  /** Forget every key the conversation no longer holds, so the map stays the seat's. */
  prune(keys: Iterable<string>): void;
}

export function sizes(): RowSizes {
  const heights = new Map<string, number>();
  return {
    sizeOf(key) {
      return key === null || key === undefined ? undefined : heights.get(key);
    },
    remember(key, height) {
      if (height > 0) heights.set(key, height);
    },
    prune(keys) {
      const held = new Set(keys);
      for (const key of heights.keys()) {
        if (!held.has(key)) heights.delete(key);
      }
    },
  };
}

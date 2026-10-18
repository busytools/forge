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

/**
 * What a held row's wrapper reserves: the height it last measured, or NOTHING
 * when it never measured at all.
 *
 * **An estimate is not a space to hold.** A held row that was never measured
 * has no size to preserve - the queued prompt's words are drawn by the pile's
 * card (rule 25: the state is drawn, by the pile), and the drain draws the row
 * fully when it starts. Reserving an estimate for one was a 386px empty box
 * standing for the whole queue wait (Ved, 2026-10-11: the black plane); the
 * #1890 shake this reserve exists for is a row that measured while it HELD its
 * words, and only that case has a number to come back at.
 */
export function heldReserve(size: number | undefined): string | undefined {
  return size === undefined ? undefined : `${size}px`;
}

/** A column's own memory of what each of its rows measured. */
interface RowSizes {
  /** What `key`'s row last measured, or `undefined` for one never measured. */
  sizeOf(key: string | null | undefined): number | undefined;
  /**
   * The middle measured height, or `undefined` before anything measured.
   *
   * **What a page is sized by**, since a turn's height is not its count: the
   * median is the column's own answer to how tall a turn here is, and the
   * page sizes ride on it rather than on a constant the transcript varies
   * wildly from (#2398). A median rather than a mean because one transcript
   * read or one giant tool output must not size every page after it.
   */
  median(): number | undefined;
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
    median() {
      if (heights.size === 0) return undefined;
      const sorted = [...heights.values()].sort((left, right) => left - right);
      const middle = sorted.length >> 1;
      const high = sorted[middle];
      if (high === undefined) return undefined;
      if (sorted.length % 2 !== 0) return high;
      return Math.round(((sorted[middle - 1] ?? high) + high) / 2);
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

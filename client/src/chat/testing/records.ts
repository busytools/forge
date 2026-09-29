/**
 * What the list was handed, in the order it was handed it.
 *
 * A module rather than a prop, because the stub is a component and the test
 * that reads this is not: this is the seam between them, and it is the whole
 * instrument - the compensation is a prop virtua reads once, at the moment the
 * row count changes, so what has to be observed is that pair.
 */
export interface Handed {
  /** How many rows the list held when it was given this. */
  length: number;
  /** Whether it was told to hold its place from the end. */
  shift: boolean;
}

export const records: Handed[] = [];

export function clear(): void {
  records.length = 0;
}

/** The last pair the list was handed. */
export function last(): Handed | undefined {
  return records[records.length - 1];
}

/** The first pair at which the list held more rows than at the one before it. */
export function firstGrowth(): Handed | undefined {
  for (let at = 1; at < records.length; at += 1) {
    const before = records[at - 1];
    const now = records[at];
    if (before !== undefined && now !== undefined && now.length > before.length) return now;
  }
  return undefined;
}

/** What the stub can be told to do once it is mounted. */
export interface ListHandle {
  /** Where the reader scrolled to, which is what drives the column. */
  scrolledTo(at: number, total: number, height: number): void;
}

let mounted: ListHandle | null = null;

export function register(handle: ListHandle | null): void {
  mounted = handle;
}

/** The list the last-mounted column is drawing into. */
export function list(): ListHandle | null {
  return mounted;
}

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

/**
 * One scroll the column asked the list for.
 *
 * The follow is a decision about the reader's place rather than a prop, so the
 * only thing a test can hold it by is the calls the column makes.
 */
export interface Pin {
  /** The offset the column asked for. */
  asked: number;
  /** Where the clamp left the reader, which is the foot when it asked for it. */
  landed: number;
}

export const pins: Pin[] = [];

/** Every pin the last-mounted column has asked for. */
export function pinned(): Pin[] {
  return pins;
}

export function clear(): void {
  records.length = 0;
  pins.length = 0;
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
  /** A clamp the browser made with nobody scrolling. */
  settled(at: number, total: number, height: number): void;
}

/**
 * What the list itself REPORTS: its model, which lags the element below.
 *
 * Nothing reads it today, and nothing is meant to: it is kept, with the
 * handle's size getters, because that is the surface a real `virtua` list
 * exposes and a stub that shed it would stop standing in for one. `element` is
 * the half the column decides from.
 */
export const reported = { size: 0, viewport: 0 };

/**
 * What the ELEMENT reports: where the reader is, and its own size.
 *
 * The browser clamps the scroll on the element, so this is the only side the
 * column may decide the foot from.
 */
export const element = { offset: 0, height: 0, viewport: 0 };

/**
 * A list that has MEASURED, with the reader at its start.
 *
 * A list has measured its rows before a column asks it anything, and a test
 * that could only hand it a size once it was mounted could not say what the
 * column does with the first page: the landing's own pin would read a list of
 * no height. Set it before the mount and the landing sees what a real one
 * would. The report and the element agree here; the divergence is a test's own
 * to introduce with `setElement`.
 */
export function setMeasured(total: number, height: number): void {
  reported.size = total;
  reported.viewport = height;
  element.height = total;
  element.viewport = height;
  element.offset = 0;
}

/** What the element reports, when the list's model is to read differently. */
export function setElement(height: number, viewport: number): void {
  element.height = height;
  element.viewport = viewport;
}

let mounted: ListHandle | null = null;

export function register(handle: ListHandle | null): void {
  mounted = handle;
}

/** The list the last-mounted column is drawing into. */
export function list(): ListHandle | null {
  return mounted;
}

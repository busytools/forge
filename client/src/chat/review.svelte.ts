/**
 * The seat's review threads, as the strip's row draws them.
 *
 * Held as the record is: one seat is on screen, so the rows are replaced
 * whole when the record moves. `null` is no row to draw at all - a read
 * that has not landed, or a seat with nothing under review - rather than a
 * row claiming the branch is clean.
 */
import type { ReviewStripRow } from '../session/view';

export class Reviews {
  #rows = $state<ReviewStripRow[] | null>(null);

  /** Take the record's read whole: what the wire says is what the row shows. */
  sync(rows: ReviewStripRow[] | null): void {
    this.#rows = rows === null ? null : rows.map((row) => ({ ...row }));
  }

  /** Whether the row draws at all. */
  anything(): boolean {
    return this.#rows !== null;
  }

  rows(): ReviewStripRow[] {
    return this.#rows ?? [];
  }

  /** How many rows are answers the reviewer has not read. */
  waiting(): number {
    return (this.#rows ?? []).filter((row) => row.waiting).length;
  }
}

/** The one seat's reviews, as its record last stated them. */
export const review = new Reviews();

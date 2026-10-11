/**
 * The seat's review threads, as the strip's row draws them.
 *
 * Held as the record is: one seat is on screen, so the rows are replaced
 * whole when the record moves. `null` is a read that has not landed - the
 * row draws nothing at all rather than claiming the branch is clean, which
 * is the same distinction the review page keeps between the two.
 */
import type { ReviewStripRow } from '../session/view';

export class Reviews {
  #rows = $state<ReviewStripRow[] | null>(null);

  /** Take the record's read whole: what the wire says is what the row shows. */
  sync(rows: ReviewStripRow[] | null): void {
    this.#rows = rows === null ? null : rows.map((row) => ({ ...row }));
  }

  /** Whether there is a review read to draw at all. */
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

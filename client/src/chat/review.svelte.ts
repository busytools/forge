/**
 * The seat's review threads, as the strip's row draws them, and the review
 * SETS its panel draws.
 *
 * Held as the record is: one seat is on screen, so the rows are replaced
 * whole when the record moves. `null` is no row to draw at all - no record
 * yet, or a tree with nothing to state - because the row's visibility is
 * the git row's own (#2203).
 */
import type { ReviewSetView, ReviewStripRow } from '../session/view';

class Reviews {
  #rows = $state<ReviewStripRow[] | null>(null);
  #sets = $state<ReviewSetView[] | null>(null);

  /** Take the record's read whole: what the wire says is what the row shows. */
  sync(rows: ReviewStripRow[] | null, sets: ReviewSetView[] | null): void {
    this.#rows = rows === null ? null : rows.map((row) => ({ ...row }));
    this.#sets = sets === null ? null : sets.map((set) => ({ ...set }));
  }

  /** Whether the row draws at all. */
  anything(): boolean {
    return this.#rows !== null;
  }

  rows(): ReviewStripRow[] {
    return this.#rows ?? [];
  }

  /** The filed rounds, oldest first, with their threads' code and states. */
  sets(): ReviewSetView[] {
    return this.#sets ?? [];
  }

  /** How many rows are answers the reviewer has not read. */
  waiting(): number {
    return (this.#rows ?? []).filter((row) => row.waiting).length;
  }
}

/** The one seat's reviews, as its record last stated them. */
export const review = new Reviews();

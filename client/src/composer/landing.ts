/**
 * The memory a page keeps of a seat's takes: which take it watched, and what
 * it has already placed.
 *
 * **A notice about a take this page never watched is not its own.** A landed
 * notice is per-seat server state that outlives its take, so a page that
 * attaches - or reloads - finds one whose words it never saw land; only a
 * take this page watched may put words in the reader's box. The memory is the
 * SEAT's: the record in hand can be another seat's between two reads, and a
 * notice it holds is one this page never watched.
 *
 * Shared rather than one per page, because the bargain is the same on every
 * surface that dictates: what a page differs in is only which of its boxes
 * the words go into.
 */

import type { Notice } from './wire';

export class Landing {
  #seat: string | null = null;
  #watched = false;
  #placed: string | null = null;

  /** Whether this page has watched the seat's take, which is what makes a
   *  notice about one its own to draw. */
  get watched(): boolean {
    return this.#watched;
  }

  /**
   * The words this page has to place, once, or `null` when there are none.
   *
   * `take` is whether the seat's record shows a take, and `notice` what it
   * holds. **A take starting clears what has been placed** - the record's
   * notice goes with it - which is what lets the reader dictate the same
   * words twice; a memory that outlived that would drop the second take's
   * words with nothing on screen saying why.
   */
  place(seat: string, take: boolean, notice: Notice | null): string | null {
    if (seat !== this.#seat) {
      this.#seat = seat;
      this.#watched = false;
      this.#placed = null;
    }
    if (take) this.#watched = true;
    if (notice === null) {
      this.#placed = null;
      return null;
    }
    if (notice.kind !== 'landed' || !this.#watched || notice.text === this.#placed) return null;
    this.#placed = notice.text;
    return notice.text;
  }
}

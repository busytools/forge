/**
 * The memory a page keeps of a seat's takes: the arm this page's own capture
 * set, and what it has already placed.
 *
 * **A page places words only for takes it began.** The arm is set at the press
 * and spent by the notice it places, so a notice arriving with no arm behind
 * it - a page that attached or reloaded with none of its own in flight - puts
 * nothing in the reader's box. The memory is the SEAT's: the record in hand
 * can be another seat's between two reads, and the arm goes with the seat
 * change.
 *
 * Shared rather than one per page, because the bargain is the same on every
 * surface that dictates: what a page differs in is only which of its boxes
 * the words go into.
 */

import type { Notice } from './wire';

export class Landing {
  #seat: string | null = null;
  /** The seat this page's own capture began a take on, while its landing is owed. */
  #armed: string | null = null;
  #placed: string | null = null;

  /**
   * This page's own capture has begun a take on `seat`: the arm its landing
   * waits on.
   *
   * **Set at the press rather than read off the record.** The record a page
   * reads is written once per painted frame (`session/live.ts`), so a take
   * whose start and end fold into one write arrives as a state with no take
   * in it at all - and a memory armed off the record then refuses the words
   * the reader spoke (the composer's own quick-release fix, Ved, 2026-10-15).
   * The arm is the SEAT's, as a page's boxes are: a take that outlives a visit
   * elsewhere still lands on the seat it began on.
   */
  arm(seat: string): void {
    this.#armed = seat;
  }

  /**
   * A take this page began that will never land: a capture that would not
   * open draws its line instead, and the arm goes with it rather than standing
   * for whatever notice arrives next.
   */
  drop(): void {
    this.#armed = null;
  }

  /**
   * Whether this page is holding the notice's words: its own take's landing
   * is on its way, or the words were placed here and the notice is the one
   * they came from. The cut row reads this, and it has to read true on every
   * read after the landing, not only the one that placed them.
   */
  holds(notice: Notice | null): boolean {
    return this.#armed !== null || (notice !== null && notice.text === this.#placed);
  }

  /**
   * The words this page has to place, once, or `null` when there are none.
   *
   * **The arm is spent by the notice, whatever kind it is** - a landed one
   * places its words, a line is the take's failure - so a later notice meets
   * no arm and places nothing. **A take starting clears what has been
   * placed** - the record's notice goes with it - which is what lets the
   * reader dictate the same words twice; a memory that outlived that would
   * drop the second take's words with nothing on screen saying why.
   */
  place(seat: string, notice: Notice | null): string | null {
    if (seat !== this.#seat) {
      this.#seat = seat;
      this.#placed = null;
    }
    if (notice === null) {
      this.#placed = null;
      return null;
    }
    const armed = this.#armed === seat;
    if (armed) this.#armed = null;
    if (!armed || notice.kind !== 'landed' || notice.text === this.#placed) return null;
    this.#placed = notice.text;
    return notice.text;
  }
}

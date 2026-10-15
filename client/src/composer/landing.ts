/**
 * The memory a page keeps of a seat's takes: the arm this page's own capture
 * set, and the words it has already placed.
 *
 * **A page places words only for takes it began.** The arm is set at the press
 * and spent by the notice it places, so a notice arriving with no arm behind
 * it - a page that attached or reloaded, or a take another connection began -
 * puts nothing in the reader's box. The arm is the SEAT's, as a page's boxes
 * are: a take that outlives a visit elsewhere still lands on the seat it began
 * on.
 *
 * Shared rather than one per page - the composer's own box holds one - because
 * the rules are the same on every surface that dictates: what a surface
 * differs in is only which of its boxes the words go into.
 */

import type { Notice } from './wire';

/**
 * What a notice IS, for a memory that has to tell one from another.
 *
 * The take's `generation` where the record carries it: two notices with the
 * same words are the same notice only when their takes are, which is what
 * tells a landing the record re-hands from a take that spoke the same words
 * again. **A record from a server older than the field falls back to kind and
 * text**, which cannot tell those two apart - accepted, and the reason the
 * number is carried wherever it crosses.
 */
function noticeIdentity(notice: Notice): string {
  if (notice.generation !== undefined) return `take:${String(notice.generation)}`;
  return notice.kind === 'line' ? `line:${notice.tone}:${notice.text}` : `landed:${notice.text}`;
}

export class Landing {
  #seat: string | null = null;
  /** The seat this page's own capture began a take on, and the notice it was set under. */
  #armed: { seat: string; under: string | null } | null = null;
  /** The notice whose words this box took, by identity. */
  #placed: string | null = null;

  /**
   * This page's own capture has begun a take on `seat`: the arm its landing
   * waits on, and the notice the record was already showing when it did.
   *
   * **Set at the press rather than read off the record.** The record a page
   * reads is written once per painted frame (`session/live.ts`), so a take
   * whose start and end fold into one write arrives as a state with no take in
   * it at all - and a memory armed off the record then refuses the words the
   * reader spoke (the composer's own quick-release fix, Ved, 2026-10-15).
   *
   * `under` is the notice still on the record at the press - the previous
   * landing, re-read by any write before the take's own start clears it. The
   * landing skips it rather than consuming the arm on it: spending the arm
   * there left the new take's own words nowhere to go.
   */
  arm(seat: string, under: Notice | null): void {
    this.#armed = { seat, under: under === null ? null : noticeIdentity(under) };
  }

  /**
   * A take this page began that will never land - a capture that would not
   * open, a take the reader abandoned - draws its line instead, and the arm
   * goes with it rather than standing for whatever notice arrives next.
   */
  drop(): void {
    this.#armed = null;
  }

  /**
   * Whether this box is holding the notice's words: its own take's landing is
   * on its way, or the words were placed here and the notice is the one they
   * came from. The cut row reads this, and it has to read true on every read
   * after the landing, not only on the one that placed them.
   */
  holds(notice: Notice | null): boolean {
    return this.#armed !== null || (notice !== null && noticeIdentity(notice) === this.#placed);
  }

  /**
   * The words this page has to place, once, or `null` when there are none.
   *
   * The rules, in the order they matter:
   *
   * - **A take starting clears what has been placed** - the record's notice
   *   goes with it - so the reader can dictate the same words twice.
   * - **A notice the arm was set UNDER is the previous take's, re-read**:
   *   skipped, not consumed, so the arm survives to the landing it belongs to.
   * - **The arm is spent by the notice, whatever kind it is** - a landed one
   *   places its words, a line is the take's failure - so a later notice meets
   *   no arm and places nothing.
   * - **Words this box already took place nothing**: the record hands the same
   *   landing over on every write, and by identity only the words' own take
   *   landing again is a landing of its own.
   *
   * What is NOT here is remembering that the words were placed: that belongs
   * to the box they went INTO, which only the caller knows - a take begun
   * under the dock puts its words in the dock, and the draft's cut row is not
   * about them (`placed`).
   */
  place(seat: string, notice: Notice | null): string | null {
    if (seat !== this.#seat) {
      this.#seat = seat;
      this.#placed = null;
    }
    if (notice === null) {
      this.#placed = null;
      if (this.#armed !== null) this.#armed.under = null;
      return null;
    }
    const identity = noticeIdentity(notice);
    if (this.#armed !== null && this.#armed.under === identity) return null;
    const armed = this.#armed !== null && this.#armed.seat === seat;
    if (armed) this.#armed = null;
    if (!armed || notice.kind !== 'landed' || identity === this.#placed) return null;
    return notice.text;
  }

  /** These words are this box's now, which is what its cut row reads. */
  placed(notice: Notice | null): void {
    if (notice === null) return;
    this.#placed = noticeIdentity(notice);
  }
}

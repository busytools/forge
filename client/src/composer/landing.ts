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
export function noticeIdentity(notice: Notice): string {
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
      // **The placed memory survives the take's start** (#1360). The words it
      // remembers are still in the box: a carried re-hand of the same notice
      // must not append them a second time, and the same words spoken into a
      // NEW take carry a new generation - a new identity - so dictating them
      // again still lands. (Only the generation-less fallback cannot tell
      // those apart, the skew its doc already accepts.)
      //
      // **The arm keeps the notice it was set under** for the same reason: a
      // whole-record read can re-hand the seat's unsent words after the
      // take's start cleared them, and that re-hand is still the notice the
      // press was made under.
      return null;
    }
    const identity = noticeIdentity(notice);
    if (this.#armed !== null && this.#armed.under === identity) return null;
    // **A carried notice never belongs to a take in flight** (#1360). The
    // seat's unsent words are what the press was made OVER, and a read
    // re-handing them mid-take must not place them over the landing that is
    // coming - nor spend the arm on them, whatever identity the arm was set
    // under (an intervening failed take moves `under` off them).
    if (this.#armed !== null && notice.kind === 'landed' && notice.carried === true) return null;
    const armed = this.#armed !== null && this.#armed.seat === seat;
    if (armed) this.#armed = null;
    if (notice.kind !== 'landed' || identity === this.#placed) return null;
    // **The seat's carried words are any page's to place** (#1360): a page
    // that reloaded between the landing and the send never armed, and these
    // are the same words it would have placed live. A LIVE landing stays the
    // dictating page's own - every other page meets it unarmed and places
    // nothing (#1358's rule, and what keeps words already sent from coming
    // back).
    if (!armed && notice.carried !== true) return null;
    return notice.text;
  }

  /** These words are this box's now, which is what its cut row reads. */
  placed(notice: Notice | null): void {
    if (notice === null) return;
    this.#placed = noticeIdentity(notice);
  }
}

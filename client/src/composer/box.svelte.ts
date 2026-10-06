/**
 * The state the box holds for one seat.
 *
 * The page hands the composer one record after another as the reader moves
 * between seats and the composer itself stays mounted across that move, so
 * everything the reader's own doing leaves behind - their words, the landing
 * they have taken, the prompt they answered - has to be kept per seat rather
 * than per component. Otherwise a seat's record is read into a box that
 * belongs to the seat the reader left: words already sent come back on the
 * next visit, and a draft one seat was typed on shows on another.
 *
 * The terminal holds the same state the same way, on `UiSession` rather than
 * on `App`: each session owns its own input editor, its parked draft and its
 * post-take notice, so switching the active session swaps the editor and
 * nothing crosses between them.
 */

import { SvelteMap } from 'svelte/reactivity';

import { subjectKey } from '../protocol';
import type { DictateAxes } from '../session/wire';
import type { SessionSlot } from '../wire/types';

/** The key one seat's box is held under, which is the subject the wire addresses it by. */
export function boxKey(slot: SessionSlot): string {
  return subjectKey({ session: slot });
}

export class Box {
  /** The reader's own words, and the command a send named. A send still on its way is not here: it is the conversation's own row (`chat/echoes.svelte`). */
  draft = $state('');
  sent = $state<string | null>(null);
  /** The prompt this box answered, while the core still lists it as waiting. */
  answered = $state<string | null>(null);
  /**
   * That prompt's own key, which is the id AND the question's index.
   *
   * A batch of questions rides one tool call, so the id alone would have the
   * dock stand down on the question AFTER the one the reader answered.
   */
  answeredKey = $state<string | null>(null);
  refusal = $state<string | null>(null);
  /**
   * Why the draft this box was drawing left the core without this reader
   * answering, drawn where the dock stood.
   *
   * The record drops the draft from `pending_asks` on the same update that
   * carries this, so the why has to be held here rather than read off the
   * draft that is gone.
   */
  ended = $state<{ tone: string; text: string } | null>(null);
  /**
   * The line a capture the SERVER never saw left behind: a microphone that
   * would not open, or a take released with no connection to send it on.
   *
   * Held here rather than folded from the record, because the record has no
   * word for it - the failure happened before anything crossed - and it is
   * cleared when a take is attempted again rather than by the reader's typing
   * alone, so a retry does not start under a stale refusal.
   */
  dictateLine = $state<{ tone: string; text: string } | null>(null);
  /**
   * The held draft this box drew last, which is what tells a stand-down for
   * THIS draft from one for the next. Not `$state`: nothing draws from it.
   */
  shownDraft: string | null = null;
  /** The line the reader's own typing has dismissed, which the next take clears. */
  dismissed = $state<string | null>(null);
  /**
   * When a landed take's words settled here: the whole of the beat's state.
   *
   * Read as a window rather than remembered as a flag a timer clears, so the
   * close cannot be lost with its timer.
   */
  beatAt = $state<number | null>(null);
  /** The draft the reader closed the list at, and which row a key would take. */
  closed = $state<string | null>(null);
  marked = $state(0);
  /**
   * The words a landing has already put here, so they land once.
   *
   * Deliberately not `$state`: nothing draws from it, and a reactive copy would
   * make the landing effect depend on what it writes.
   */
  landed: string | null = null;
  /**
   * Whether this box has watched its seat hold a take, ever.
   *
   * A landed notice is per-seat server state that outlives its take, so a
   * client that attaches - or reloads - finds one whose words it never saw
   * land. Only a take this box watched may put words in it, and the trade is
   * that dictation landing unwatched, and never sent, is not handed back.
   */
  sawTake = false;
  /** Whether the seat's current take has captured its destination yet. */
  takeOpen = false;
  /**
   * Which box a take's words belong to, captured when the take STARTS: the
   * dock when it held the slot with a words row, the composer's draft
   * otherwise. A prompt arriving or leaving mid-take cannot move words that
   * were spoken for the box the reader was in.
   */
  tookFrom: 'dock' | 'composer' | null = null;
  /**
   * The dictation axes this seat was last set to, or `null` while it has not
   * been edited - which is what makes the config's own value the default.
   *
   * The value is remembered on this machine under the seat's key, so what is
   * held here is the current run's copy of it: a reload reads the stored one
   * back through `axesFor`.
   */
  axes = $state<DictateAxes | null>(null);
  /** The input this seat records from, or `null` for the system default. */
  device = $state<string | null>(null);
}

/** The boxes one composer holds, one per seat, made the first time a seat is shown. */
export class Boxes {
  #held = new SvelteMap<string, Box>();

  /**
   * The box for a seat, which is the one it was handed before if it has been
   * shown already - a reader returning to a seat meets the words they left.
   */
  of(key: string): Box {
    const held = this.#held.get(key);
    if (held !== undefined) return held;
    const made = new Box();
    this.#held.set(key, made);
    return made;
  }

  /**
   * The box a seat already holds, or `undefined` for one nothing has drawn -
   * an update for such a seat has no dock behind it to explain, and minting a
   * box would keep its line for a page that never showed it.
   */
  held(key: string): Box | undefined {
    return this.#held.get(key);
  }
}

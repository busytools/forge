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
 * A view holds the same state the same way, on its own session record rather
 * than on the app: each session owns its own input editor, its parked draft
 * and its post-take notice, so switching the active session swaps the editor
 * and nothing crosses between them.
 */

import { SvelteMap } from 'svelte/reactivity';

import { subjectKey } from '../protocol';
import type { DictateAxes } from '../session/wire';
import type { SessionSlot } from '../wire/types';
import { Landing } from './landing';

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
  /**
   * The held browser hand-off this box drew last, on [`shownDraft`]'s own
   * terms: what tells a stand-down for THIS hand-off from one for the next.
   */
  shownHandOff: string | null = null;
  /**
   * The profile that hand-off named, kept beside its id so an answer from
   * ANOTHER view can lower the window this view raised: the resolved update
   * carries the id and the ending, never the profile.
   */
  shownHandOffProfile: string | null = null;
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
   * The words this box's draft took, as the notice carried them: what its cut
   * row reads.
   *
   * **The take's own identity, not its words** (`noticeIdentity`): two takes
   * speaking the same text are two landings, and a text comparison would draw
   * the row for one whose words went elsewhere. `$state` and not a plain
   * field, because the row has to appear the moment the words land somewhere
   * this box re-renders from. The arm used to stand in for that memory and
   * drew the row on the bare press, which is the Cmd jerk (measured live,
   * 2026-10-15).
   */
  landed = $state<string | null>(null);
  /**
   * This box's landing memory: the arm its own take's press set, the notice it
   * was set under, and the words this DRAFT took.
   *
   * One per box, which is one per seat: a take that outlives a visit elsewhere
   * still lands on the seat it began on. The rules live in `landing.ts`, where
   * the review and document pages keep the same memory; what is this box's own
   * is only which of its boxes the words went into.
   *
   * **The guard is load-bearing, not decorative.** Take news goes to the
   * connection that began the take, and the connection outlives a route
   * change: a take pressed on one surface keeps running after the reader
   * moves, and its notice reaches whatever surface is mounted then - this
   * box's arm is what keeps words this box never began out of its draft.
   */
  landing = new Landing();
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

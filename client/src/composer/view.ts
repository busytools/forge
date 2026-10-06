/**
 * The composer's reads gathered into what its markup wants - the client half of
 * the gather `crates/forge-web/src/composer.rs` does across its own render
 * functions.
 *
 * Pure, so a test can build a composer by hand. Nothing here recomputes a state
 * the core already decided: the take, the notice, the compaction, the sign-in
 * and the prompt all arrive in the record, and a state this module derives is
 * derived from the reader's own doing - what they typed, what they sent, and
 * which seat they are looking at.
 */

import type { Connection } from '../socket';
import type { DictateWire, Lifecycle } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { askFrom, composerFrom, type Ask, type ComposerState, type Notice } from './wire';

/**
 * The seat behind this page, as the composer needs it.
 *
 * A blocking state is the seat's rather than the socket's, so the state and the
 * reason it is in one come from the page's own read of the roster: `waking` for
 * a seat nothing has started, and the lifecycle for one that is coming up,
 * failing or on its way out - a boolean cannot tell a spawn from a failure.
 */
export interface SeatRead {
  /**
   * The core's lifecycle, or `null` for a seat the roster does not name.
   *
   * Null and `waking` say the same thing from two sides, and this component
   * treats them as one state rather than letting two fields disagree about it.
   */
  lifecycle: Lifecycle | null;
  reason: string | null;
  /** The roster names no row for this seat, so nothing is running behind it. */
  waking: boolean;
  /**
   * How many prompts this seat is holding, the one on screen included.
   *
   * The core arbitrates the queue, so what a dock can say about the ones behind
   * it is the depth the roster reported - never a queue this client keeps.
   */
  pendingDepth: number;
}

/**
 * What the composer reads of one session's record.
 *
 * A structural subset of the session page's `SessionRecord`: the page owns the
 * subscription and hands the composer the record it already holds, and the
 * fields here are the ones this component reads. They are REQUIRED rather than
 * optional, because a read that arrives as `undefined` draws a dropdown with no
 * rows and reads as a search that found nothing.
 */
export interface ComposerRecord {
  /**
   * The seat this record is for.
   *
   * The page keeps this composer mounted while it hands it one seat's record
   * after another, and between a switch and the new seat's first record the
   * record in hand is the PREVIOUS seat's. Everything a record writes is gated
   * on this matching the seat being shown, so a landing that belongs to the
   * seat being left cannot land in the box being moved to.
   */
  slot: SessionSlot;
  /**
   * What this seat's composer is doing, as the wire sends it.
   *
   * Left as it came rather than narrowed by the page: the take, the notice and
   * the sign-in are the composer's own states, so the component that draws them
   * is the one that narrows them, and no reader upstream has to re-narrow what
   * only this one acts on.
   */
  composer: unknown;
  /**
   * The prompts this seat is holding - a draft leads, then arrival order -
   * or `[]` when nothing waits. The dock draws the front; the ones behind it
   * are what the depth line counts.
   */
  pending_asks: unknown[];
  header: { turn_in_flight: boolean };
  /** The slash commands the CLI last advertised for this seat. */
  slash_commands: unknown;
  /** The subagent types the CLI catalogue names. */
  subagents: unknown;
  /** This seat's working tree, walked once and shared with the cache. */
  file_index: unknown;
}

/**
 * What the composer is handed, which is what the session page's `composer`
 * snippet renders with.
 *
 * The connection is narrowed to what this component uses of one: it sends
 * commands, it hears the refusals that come back, and it reads a seat's own
 * store where a decision needs the state as it is now rather than as the
 * record last drew it. A page handing it a whole `Connection` satisfies that
 * structurally.
 */
/**
 * The socket, as the composer and its panel use it: commands and drafts
 * through `dispatch`, the picker through `devices`, and - since dictation
 * moved to this side - the two a take reads (`status`, `onStatus`), the
 * binary path (`frame`) and the settings the axes reset to.
 */
export type ComposerConnection = Pick<
  Connection,
  'dispatch' | 'onMessage' | 'devices' | 'store' | 'settings' | 'status' | 'onStatus' | 'frame'
>;

export interface ComposerProps {
  record: ComposerRecord;
  slot: SessionSlot;
  connection: ComposerConnection;
  seat: SeatRead;
  /**
   * Whether this install can dictate at all.
   *
   * A home read rather than a session one - the engine is process-wide - so
   * whoever mounts the composer states it from the home wire. **Required, and
   * that is the point**: it was optional first, which is how the mount came to
   * draw no mic for a whole round without anything failing. A control this
   * install cannot honour is worse than none, but a silent default is worse
   * than both.
   */
  dictation: boolean;
}

/**
 * Whether dictation is on offer: on, and every declared model LOADED.
 *
 * `enabled` is carried rather than inferred from an empty model list, because
 * that list is empty both for a switched-off `[dictate]` and for a snapshot
 * taken before the models loaded - so both are read and neither is assumed.
 *
 * **A model is loaded when it is `ready`, which is the same reading the home's
 * own card takes** (`home/view.ts` counts `state === 'ready'` and calls the
 * install ready only when every model is). Anything less draws a mic whose
 * click dispatches a `dictate_start` the core cannot honour, because the engine
 * is parked only when the whole set loaded. The mic still arrives - the page
 * re-reads the home when `DictateAvailability` lands - so the wait is a delay
 * rather than an absence.
 */
export function dictationOffered(dictate: DictateWire): boolean {
  const models = dictate.snapshot.models;
  return dictate.enabled && models.length > 0 && models.every((model) => model.state === 'ready');
}

/** The composer's own state, read off the record the page handed it. */
export function composerState(record: ComposerRecord): ComposerState {
  return composerFrom(record.composer);
}

/**
 * The prompt the dock draws: the front of the seat's queue, which is the
 * core's answer and never the client's. A parallel batch parks several at
 * once, and the front is the oldest - the terminal's own rule for its queue.
 */
export function pendingAsk(record: ComposerRecord): Ask | null {
  return askFrom(record.pending_asks[0]);
}

/**
 * A state that replaces the box entirely: the seat is not taking input, and
 * this is why.
 *
 * The order is the one a reader has to be told, which is the terminal's own:
 * a seat that is failing is told that first, a seat nothing has started cannot
 * tell whether a session is coming up or never will, and a compaction is the
 * more specific truth about a turn the reader sent.
 */
export interface Blocker {
  line: string;
  /** What to do about it, when there is something to do. */
  sub: string | null;
  /** Whether the line reports a failure rather than a wait. */
  bad: boolean;
  /** Whether the line is a wait, which draws the ring. */
  waiting: boolean;
}

/** The line the box's own key hints draw, where the mockup draws them. */
export const SIGN_IN_FALLBACK = 'Run `claude auth login` in another terminal to authenticate';

/** A blocked state draws no ring, and one that reports a failure draws none either. */
const STOPPED: Pick<Blocker, 'bad' | 'waiting'> = { bad: false, waiting: false };

/**
 * The reason the seat takes no input, or `null` when it does.
 *
 * `command` is the slash command the reader sent and the turn still working on
 * it, which is the terminal's own rule: a long command is a wait the reader
 * caused, and naming it is what tells them what they are waiting for.
 */
export function blocked(
  seat: SeatRead,
  composer: ComposerState,
  command: string | null,
): Blocker | null {
  // Sign-in keeps its box: the hint above it is where the reader is told.
  if (seat.lifecycle === 'AuthRequired') return null;
  if (seat.lifecycle === 'Failed') {
    return {
      line: 'Input disabled due to error',
      sub: seat.reason ?? 'Quit forge and start it again to recover this session.',
      bad: true,
      waiting: false,
    };
  }
  // A seat with no lifecycle is one nothing has started, which is the same
  // state `waking` names - so the two cannot draw different reasons for it.
  if (
    seat.waking ||
    seat.lifecycle === null ||
    seat.lifecycle === 'Sleeping' ||
    seat.lifecycle === 'LoggedOut'
  ) {
    return {
      line: 'not running',
      sub: seat.reason ?? 'this seat has no session behind it',
      ...STOPPED,
    };
  }
  if (seat.lifecycle === 'Spawning') {
    return { line: 'Connecting to Claude Code...', sub: null, bad: false, waiting: true };
  }
  if (composer.compacting) {
    return { line: 'Compacting context...', sub: null, bad: false, waiting: true };
  }
  if (command !== null) {
    return { line: `Running ${command}`, sub: null, bad: false, waiting: true };
  }
  return null;
}

/** The sign-in method's own words for how to authenticate, when the core sent them. */
export function signInLine(composer: ComposerState): string {
  const described = composer.signIn?.methodDescription ?? '';
  return described.trim() === '' ? SIGN_IN_FALLBACK : described;
}

/** What a take says when it hit its cap and landed only part of what was said. */
export const TRUNCATED = 'this is what fitted · keep going from the end';

/**
 * The line a notice draws, or `null` when it draws words instead.
 *
 * A take that landed draws its words in the box rather than a line about them,
 * which is the terminal's own rule: the words ARE the notice. One that landed
 * part of a capped take says so as well, because the reader has to know the
 * take was cut rather than that they stopped speaking - but a notice this
 * composer never saw a take for draws neither, since its row would be a note
 * about words that are not in the box.
 */
export function noticeLine(
  notice: Notice | null,
  sawTake: boolean,
): { tone: string; text: string } | null {
  if (notice === null) return null;
  if (notice.kind === 'line') return { tone: notice.tone, text: notice.text };
  if (!sawTake) return null;
  return notice.truncated ? { tone: 'warn', text: TRUNCATED } : null;
}

/**
 * What one draft ending says, drawn where the dock stood.
 *
 * The dock is the reader's answer being asked for; when the draft leaves the
 * core without their answer, this is the only thing that says what became of
 * it. A draft is answered by its own id in whichever view, so "another view"
 * is the honest subject: this one did not answer it, or its dock would be
 * gone by its own doing.
 */
export function draftEndingLine(ending: unknown): { tone: string; text: string } {
  if (ending === 'expired') {
    return { tone: 'q', text: 'The Slack draft expired unanswered.' };
  }
  if (ending === 'abandoned') {
    return { tone: 'q', text: "The Slack draft's asking session went away." };
  }
  const answered = enumField(ending, 'answered');
  if (answered !== undefined) {
    return {
      tone: 'q',
      text:
        enumField(answered, 'approved') === true
          ? 'The Slack draft was posted from another view.'
          : 'The Slack draft was declined in another view.',
    };
  }
  // An ending this client is older than still means the draft is gone, and
  // saying less than that would leave the dock's disappearance unexplained.
  return { tone: 'q', text: 'The Slack draft is no longer waiting.' };
}

/**
 * What one browser hand-off ending says, drawn where the dock stood - the
 * draft line's own twin, and the same subject: this view did not answer it,
 * or its dock would be gone by its own doing.
 */
export function handOffEndingLine(ending: unknown): { tone: string; text: string } {
  const kind = enumField(ending, 'type');
  if (kind === 'done') {
    return { tone: 'q', text: 'The browser hand-off was settled in another view.' };
  }
  if (kind === 'not_now') {
    return { tone: 'q', text: 'The browser hand-off was declined in another view.' };
  }
  if (kind === 'abandoned') {
    return { tone: 'q', text: "The browser hand-off's asking session went away." };
  }
  // An ending this client is older than still means the hand-off is gone, and
  // saying less than that would leave the dock's disappearance unexplained.
  return { tone: 'q', text: 'The browser hand-off is no longer waiting.' };
}

/** One field of an externally tagged enum's variant, or `undefined` for a unit variant. */
function enumField(value: unknown, name: string): unknown {
  return typeof value === 'object' && value !== null
    ? (value as Record<string, unknown>)[name]
    : undefined;
}

/**
 * A take's words at the end of the draft, which is where the reader was about
 * to type.
 *
 * Ported from `joined` in `crates/forge-web/src/composer.rs`, whitespace rule
 * included: words go straight onto a draft that already ends in space, so a
 * take after a newline does not open with one.
 */
export function joined(draft: string, words: string): string {
  if (draft === '' || /\s$/.test(draft)) return `${draft}${words}`;
  return `${draft} ${words}`;
}

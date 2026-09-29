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
   * What this seat's composer is doing, as the wire sends it.
   *
   * Left as it came rather than narrowed by the page: the take, the notice and
   * the sign-in are the composer's own states, so the component that draws them
   * is the one that narrows them, and no reader upstream has to re-narrow what
   * only this one acts on.
   */
  composer: unknown;
  /** The prompt this seat is parked on, or `null` when nothing waits. */
  pending_ask: unknown;
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
 * commands and it hears the refusals that come back, and a page handing it a
 * whole `Connection` satisfies that structurally.
 */
export interface ComposerProps {
  record: ComposerRecord;
  slot: SessionSlot;
  connection: Pick<Connection, 'dispatch' | 'onMessage'>;
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
 * Whether dictation is on offer, from the home wire's own two halves.
 *
 * `enabled` is carried rather than inferred from an empty model list, because
 * that list is empty both for a switched-off `[dictate]` and for a snapshot
 * taken before the models loaded - so both are read and neither is assumed.
 */
export function dictationOffered(dictate: DictateWire): boolean {
  return dictate.enabled && dictate.snapshot.models.length > 0;
}

/** The composer's own state, read off the record the page handed it. */
export function composerState(record: ComposerRecord): ComposerState {
  return composerFrom(record.composer);
}

/** The prompt the seat is parked on, which is the core's answer and never the client's. */
export function pendingAsk(record: ComposerRecord): Ask | null {
  return askFrom(record.pending_ask);
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
    return { line: 'Connecting to Claude Code…', sub: null, bad: false, waiting: true };
  }
  if (composer.compacting) {
    return { line: 'Compacting context…', sub: null, bad: false, waiting: true };
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
 * take was cut rather than that they stopped speaking.
 */
export function noticeLine(notice: Notice | null): { tone: string; text: string } | null {
  if (notice === null) return null;
  if (notice.kind === 'line') return { tone: notice.tone, text: notice.text };
  return notice.truncated ? { tone: 'warn', text: TRUNCATED } : null;
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

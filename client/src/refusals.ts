import { subjectKey } from './protocol';
import type { SessionSlot } from './wire/types';

/**
 * One line the reader never got: a dispatch this client refused to send.
 *
 * The socket owns the refusal and the seat's column owns the draw, and they
 * are separate component trees - so the line travels as a listener set rather
 * than as a prop, the same shape `socket.ts` uses for its message and status
 * listeners.
 *
 * **The seat is the one the click was MADE in, which the caller states** - a
 * rail's close targets another seat than the column the reader is looking at,
 * and a send refused in a seat whose column is open draws into that kept
 * conversation whether or not the reader is on it; a seat this client never
 * opened has no conversation and the refusal draws nowhere. Nothing is held
 * across the note: like the other live-only notices, a reload does not
 * replay it.
 */
export interface Refusal {
  /** The seat the click was made in, keyed the way a subject keys. */
  seat: string;
  text: string;
}

const listeners = new Set<(line: Refusal) => void>();

/**
 * What the reader is told when a command never left the browser.
 *
 * One line for both surfaces that say it: the seat's column draws it as a
 * notice, and a message that never went carries it as its own reason.
 */
export const NOT_SENT = 'Not sent - the connection is down.';

/**
 * The reason a message row carries when the link died under it.
 *
 * The row draws it beside the words the retry hands back, so the sentence is
 * the short fact rather than the notice's full line.
 */
export const CONNECTION_DOWN = 'the connection is down';

/** Draw the next refusal in the given seat's column; returns the off switch. */
export function onRefusal(fn: (line: Refusal) => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

/**
 * Note that a command never left the browser, for the seat the reader was in.
 *
 * The words say what happened rather than what threw: every refusal this path
 * takes today is the socket being down, and "the socket is not open" is the
 * code's own phrasing, not the reader's.
 */
export function refused(at: SessionSlot): void {
  const line: Refusal = {
    seat: subjectKey({ session: at }),
    text: NOT_SENT,
  };
  for (const fn of listeners) fn(line);
}

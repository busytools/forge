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
    text: 'Not sent - the connection is down.',
  };
  for (const fn of listeners) fn(line);
}

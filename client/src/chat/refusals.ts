import { subjectKey } from '../protocol';
import type { SessionSlot } from '../wire/types';

/**
 * One line the reader never got: a dispatch this client refused to send.
 *
 * The socket owns the refusal and the seat's column owns the draw, and they
 * are separate component trees - so the line travels as a listener set rather
 * than as a prop, the same shape `socket.ts` uses for its message and status
 * listeners. Nothing is held: a refusal belongs to the conversation that was
 * on screen when the click happened.
 */
export interface Refusal {
  /** The seat the command addressed, keyed the way a subject keys. */
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
 * Note that a command never left the browser.
 *
 * The words say what happened rather than what threw: every refusal this path
 * takes today is the socket being down, and "the socket is not open" is the
 * code's own phrasing, not the reader's.
 */
export function refused(seat: SessionSlot | null): void {
  if (seat === null) return;
  const line: Refusal = {
    seat: subjectKey({ session: seat }),
    text: 'Not sent - the connection is down.',
  };
  for (const fn of listeners) fn(line);
}

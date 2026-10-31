/**
 * Which seats owe their next snapshot to a return's own re-read.
 *
 * **A return asks its shown seat for a fresh record, and that answer is the
 * returner's only word on what happened while the page was away** - a prompt
 * another client queued announces itself to nobody here. The chat arms its
 * queue hold from a snapshot's listing, but refuses an ordinary mid-stream
 * snapshot because a read the server built before a send can be delivered
 * after it. So the read a return asked is named here, per connection and
 * seat, and spent by the first snapshot that lands for it: the guard stays
 * for every snapshot no return asked for.
 *
 * **The mark is a seat's, not a page's.** It is set where the read is asked
 * (`connect/resync.ts`), so it exists whether or not a chat column is mounted,
 * and a seat's mark cannot arm another seat's snapshot.
 */

import { subjectKey } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

const RETURNS = new WeakMap<Connection, Set<string>>();

/** Name `slot` as one whose next snapshot answers a return's own read. */
export function markReturn(connection: Connection, slot: SessionSlot): void {
  const key = subjectKey({ session: slot });
  const held = RETURNS.get(connection);
  if (held === undefined) RETURNS.set(connection, new Set([key]));
  else held.add(key);
}

/** Whether `slot` owes a return read, spending the mark when it does. */
export function takeReturn(connection: Connection, slot: SessionSlot): boolean {
  const held = RETURNS.get(connection);
  return held !== undefined && held.delete(subjectKey({ session: slot }));
}

/**
 * Drop a seat's mark without spending it.
 *
 * **A drop replaces it with a stronger fact.** The reconnect's snapshot is
 * the first word after the socket died, so the chat's `missed` already makes
 * it authoritative - and a mark outliving the read it named would arm a
 * later, ordinary snapshot instead.
 */
export function clearReturn(connection: Connection, slot: SessionSlot): void {
  RETURNS.get(connection)?.delete(subjectKey({ session: slot }));
}

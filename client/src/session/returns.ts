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
 * **The mark is a seat's, not a page's.** It is set by `session/live.ts` when
 * the refresh actually goes out - `connect/resync.ts` only passes the intent
 * through - so it exists whether or not a chat column is mounted, and a
 * seat's mark cannot arm another seat's snapshot.
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

/**
 * Whether `slot` owes a return read, spending the mark when it does.
 *
 * **The next snapshot spends it**, whatever made that snapshot authoritative:
 * a drop's reconnect answer is numbered the same way a return's is, and the
 * chat spends the mark before it asks whether `missed` or `!landed` already
 * said yes. No separate clearing is needed on a drop.
 */
export function takeReturn(connection: Connection, slot: SessionSlot): boolean {
  const held = RETURNS.get(connection);
  return held !== undefined && held.delete(subjectKey({ session: slot }));
}

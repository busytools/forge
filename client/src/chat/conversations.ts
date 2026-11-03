/**
 * The conversations one connection is holding, kept for its life.
 *
 * **Module state, not the column's.** The Chat column unmounts when the
 * reader leaves the session page, and the folded conversation must not go
 * with it: a return is owed what the frames kept current, with no read of
 * the seat. Keyed by the connection because a held conversation belongs to
 * the socket whose frames fold into it - a new connection starts its own.
 *
 * **A released seat takes its conversation with it** ([`releaseConversation`]):
 * the column's listeners stop, so a same-slot successor's frames are not
 * folded into a conversation nothing draws.
 */
import { SvelteMap } from 'svelte/reactivity';

import type { Connection } from '../socket';
import type { Anchor } from './anchor';
import type { Chat, Conversation } from './conversation';

/** One seat's open conversation and the column that keeps folding it. */
export interface HeldChat {
  connection: Connection;
  chat: Chat;
  stop: () => void;
}

/** What one connection holds for its seats. */
export interface Conversations {
  /** Each seat's last drawn conversation, so a return paints it at once. */
  kept: SvelteMap<string, Conversation>;
  /** Each seat's open conversation, which keeps folding after the page leaves. */
  live: SvelteMap<string, HeldChat>;
  /**
   * Where each seat's reader was parked when the column was last on it.
   *
   * **A place outlives the column, the way the conversation does** (#2834):
   * a document unmounts the page, and where the element was scrolled is not in
   * the kept record. A seat with no entry was left at its foot, which needs no
   * restoring.
   */
  places: Map<string, Anchor>;
  /**
   * The seat the column last drew for this connection, which is what tells a
   * return to a seat from a seat switch: only a mount onto this seat takes its
   * place back, and a switch to any other lands at the latest (#1673).
   */
  shown: string | null;
}

const HELD = new WeakMap<Connection, Conversations>();

/** The conversation maps for `connection`, made on first use. */
export function conversationsFor(connection: Connection): Conversations {
  const held = HELD.get(connection);
  if (held !== undefined) return held;
  const fresh: Conversations = {
    kept: new SvelteMap(),
    live: new SvelteMap(),
    places: new Map(),
    shown: null,
  };
  HELD.set(connection, fresh);
  return fresh;
}

/**
 * Release one seat's conversation: its column's listeners stop with it, and
 * nothing is left to fold a successor's frames into.
 *
 * **Only that seat's entry goes.** The connection's others are the rest of
 * the fleet the reader has visited, and they stay current.
 */
export function releaseConversation(connection: Connection, key: string): void {
  const held = HELD.get(connection);
  if (held === undefined) return;
  held.live.get(key)?.stop();
  held.live.delete(key);
  held.kept.delete(key);
  held.places.delete(key);
  if (held.shown === key) held.shown = null;
}

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
import type { Chat, Conversation } from './conversation';

/** One seat's open conversation and the column that keeps folding it. */
export interface HeldChat {
  connection: Connection;
  chat: Chat;
  stop: () => void;
}

/** The two maps one connection holds. */
export interface Conversations {
  /** Each seat's last drawn conversation, so a return paints it at once. */
  kept: SvelteMap<string, Conversation>;
  /** Each seat's open conversation, which keeps folding after the page leaves. */
  live: SvelteMap<string, HeldChat>;
}

const HELD = new WeakMap<Connection, Conversations>();

/** The conversation maps for `connection`, made on first use. */
export function conversationsFor(connection: Connection): Conversations {
  const held = HELD.get(connection);
  if (held !== undefined) return held;
  const fresh: Conversations = { kept: new SvelteMap(), live: new SvelteMap() };
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
}

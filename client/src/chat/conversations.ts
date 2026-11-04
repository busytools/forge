/**
 * The conversations one connection is holding, kept for its life, and the
 * places its readers parked at, kept for the app run.
 *
 * **Module state, not the column's.** The Chat column unmounts when the
 * reader leaves the session page, and the folded conversation must not go
 * with it: a return is owed what the frames kept current, with no read of
 * the seat. Keyed by the connection because a held conversation belongs to
 * the socket whose frames fold into it - a new connection starts its own.
 *
 * **A saved place outlives the connection as well as the column.** A place is
 * a fact about a conversation rather than about a socket: a reconnect builds a
 * new connection and a new Chat, and the reader is owed the row they left
 * either way. The store is [`places`], keyed by the seat and tagged with the
 * claude session id the place was recorded in, so a `/new` or a `/resume` to a
 * different occupant is a different conversation and the old place is not
 * its own.
 *
 * **A released seat takes its conversation with it** ([`releaseConversation`]):
 * the column's listeners stop, so a same-slot successor's frames are not
 * folded into a conversation nothing draws. Its place stays: the successor may
 * be the same occupant reconnecting.
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
}

/**
 * Where one conversation's reader was parked, and the occupant it belongs to.
 *
 * The row is `Anchor`'s - the row the reader's eye was on and how far into it
 * they were - and `session` is the claude session id the conversation ran as
 * when it was recorded. A place recorded before any id was known is the seat's
 * whatever occupant arrives, because there was no other conversation to
 * confuse it with; one whose id disagrees with the conversation now on the
 * seat is not that conversation's.
 */
export interface Place {
  session: string | null;
  anchor: Anchor;
}

/**
 * The saved place of every conversation the reader has parked on this run.
 *
 * **One entry per seat.** A slot has one occupant at a time, so the seat names
 * the conversation and the session id beside it says whether the conversation
 * is still the same one. Two entries for one seat would only ever be an old
 * occupant's, which is exactly the place `/new` must not reopen.
 */
export class Places {
  readonly #held = new Map<string, Place>();

  /** Where the reader of `seat` was parked, or `undefined` for a foot. */
  of(seat: string): Place | undefined {
    return this.#held.get(seat);
  }

  /** Record where the reader of `seat` stopped, in the session it is running. */
  keep(seat: string, session: string | null, anchor: Anchor): void {
    this.#held.set(seat, { session, anchor });
  }

  /**
   * The seat is at its foot, or holds no row its reader does: no place.
   *
   * **An occupant swap is the third case**, and it is not a foot: a `/new` or
   * a `/resume` replaces the conversation the place belonged to, and the
   * successor must open at its latest.
   */
  forget(seat: string): void {
    this.#held.delete(seat);
  }
}

/** Every seat's saved place, for the life of the app run. */
export const places = new Places();

const HELD = new WeakMap<Connection, Conversations>();

/** The conversation maps for `connection`, made on first use. */
export function conversationsFor(connection: Connection): Conversations {
  const held = HELD.get(connection);
  if (held !== undefined) return held;
  const fresh: Conversations = {
    kept: new SvelteMap(),
    live: new SvelteMap(),
  };
  HELD.set(connection, fresh);
  return fresh;
}

/**
 * Release one seat's conversation: its column's listeners stop with it, and
 * nothing is left to fold a successor's frames into.
 *
 * **Only that seat's entry goes.** The connection's others are the rest of
 * the fleet the reader has visited, and they stay current. The saved place
 * stays too: a reconnect is a new connection over the same conversation, and
 * the reader is owed the row they left.
 */
export function releaseConversation(connection: Connection, key: string): void {
  const held = HELD.get(connection);
  if (held === undefined) return;
  held.live.get(key)?.stop();
  held.live.delete(key);
  held.kept.delete(key);
}

/**
 * The session over a live connection: the record it is answered with, and the
 * update it applies to it.
 *
 * **The update is the news, and it is already everything a page needs.** The
 * terminal applies each variant to the state it holds, which is why it keeps
 * up with a busy seat that this page used to fall behind: answering every
 * update with a read is a full encode on the server - the transcript re-folded
 * into turns, the process tree walked, the working tree scanned - so the page
 * ran further behind the busier the seat was. A read is for a cold load, for a
 * reconnect, and for the slices no update carries.
 *
 * The subject is the SEAT, and the connection routes an update to it by the
 * slot the update carries, so a store's contents are this seat's and nothing
 * else's.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { slotOf, subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import type { SessionSlot } from '../wire/types';
import { applyUpdate, REPLACES, UNFED, variantOf } from './apply';
import { sessionFrom, type SessionRecord } from './wire';

/**
 * How often the slices no update carries are read.
 *
 * A read is a whole encode, and the six of them are the slowest-moving part of
 * the record - a git scan and a process walk do not change between one frame
 * and the next - so this is the coarsest read that keeps a page honest rather
 * than the cheapest that keeps it moving.
 */
export const POLL_MS = 5_000;

/** What the session page has to draw from, and why it has nothing when it does not. */
export interface SessionRead {
  wire: SessionRecord | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: SessionRead = { wire: null, refused: null };

/**
 * Watch one seat.
 *
 * The subscription, the message listener and the status listener are all let go
 * with the last subscriber, so a page that has moved to another seat is not
 * left subscribed to the one it left.
 */
export function watchSession(
  connection: Connection,
  slot: SessionSlot,
  /**
   * Whether this page can ANSWER the prompts it draws.
   *
   * **It is not a client's preference, and getting it wrong loses a turn
   * either way.** A client counted as answering that cannot display a prompt
   * hangs the turn, because the core parks it on a reply that never comes; a
   * client drawing a dock while subscribed as an observer has every prompt it
   * shows cancelled. So the caller states what it has: the page says yes
   * exactly when its composer - the thing with the dock in it - is wired in.
   */
  answering: boolean,
): Readable<SessionRead> {
  const subject = { session: slot };
  let held: Store | null = null;
  // A read is a full encode on the server, so asking again while one is in
  // flight queues them behind each other.
  let reading = false;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  /**
   * The record as it is held, which is what an update is applied to.
   *
   * Held beside the store rather than read back out of it: the reducer needs
   * the value before this one, and a store is what a page reads, not a place
   * to compute from.
   */
  let wire: SessionRecord | null = null;

  /**
   * Whether the next answer is a whole record rather than a poll's.
   *
   * Three things make one: the first read, a reconnect (its subscription is
   * re-made, so its snapshot is the server's own account of where the seat is
   * now), and a seat that spawned, connected or took a new occupant. Every
   * other answer is a poll's, and a poll is only allowed to move what no
   * update feeds.
   */
  let replacing = true;

  /** A poll's timer, which is armed with this page's subscription. */
  let poll: ReturnType<typeof setInterval> | null = null;

  function publish(next: SessionRecord | null, refused: string | null): void {
    wire = next;
    view.set({ wire: next, refused });
  }

  function read(replace: boolean): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      publish(null, state.why);
      return;
    }
    const data = held.snapshot();
    if (data === null) return;
    const fresh = sessionFrom(data);
    publish(replace || wire === null ? fresh : merged(wire, fresh), null);
  }

  /**
   * A poll's answer, keeping only the slices no update carries.
   *
   * **The conversation is the one to watch here.** A poll is asked for while
   * frames are arriving, and its answer was encoded after some of them and
   * before others - so taking its whole record would drop the frames that
   * landed in between, and the page would walk backwards on a busy seat.
   */
  function merged(held: SessionRecord, fresh: SessionRecord): SessionRecord {
    const out = { ...held };
    for (const field of UNFED) {
      Object.assign(out, { [field]: fresh[field] });
    }
    return out;
  }

  /**
   * Ask the server for the record again.
   *
   * Nothing is asked while one ask is in flight, and a drop takes the ask with
   * it: the reconnect answers with a snapshot of its own, which is fresher than
   * this one would have been.
   */
  function reread(): void {
    if (reading) return;
    reading = true;
    connection.refresh(subject);
  }

  function watch(): void {
    held = connection.subscribe(subject, { answering });
    // Compared by KEY rather than by the subject itself: a seat's subject is
    // an object, and the one the server answers with is a different object
    // that happens to say the same thing. `===` never matches it, so the page
    // would draw the home's own sections and none of the seat's.
    const key = subjectKey(subject);
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        if (message.kind === 'error' || subjectKey(message.subject) === key) {
          reading = false;
          read(replacing);
          replacing = false;
        }
        return;
      }
      if (message.kind !== 'update') return;
      // **The connection is not this seat's alone.** The shell holds the home
      // for its whole life, and the server filters per CONNECTION over the
      // union of the subjects watched, so every other seat's frames arrive
      // here too. This is the server's own covering rule for a session - a
      // seat hears an update when the update's slot is that seat - and
      // without it a busy fleet would write another seat's frames into this
      // page's record.
      const at = slotOf(message.update);
      if (at === null || subjectKey({ session: at }) !== key) return;

      // The three variants that REPLACE the record rather than patching it: a
      // seat waking up, connecting, or taking a new occupant. What they carry
      // is not a record - the folded transcript, the process walk, the working
      // tree - so only a read answers them.
      const [name] = variantOf(message.update);
      if (name !== null && REPLACES.includes(name)) {
        replacing = true;
        reread();
        return;
      }

      // Nothing to apply to yet: the first read has not landed, and the frames
      // to come are answered by it.
      if (wire === null) return;
      const next = applyUpdate(wire, message.update);
      // An update this record has nothing to do with: publishing it would
      // redraw every reader of the page for no change at all.
      if (next === wire) return;
      publish(next, null);
    });
    // A drop takes any read in flight with it, and the reconnect answers with a
    // snapshot of its own - so the pacing must not stay stuck waiting on an
    // answer that died.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        reading = false;
        // The record this page holds is from before the drop, so the answer
        // the reconnect brings is a whole one rather than a poll's.
        replacing = true;
      }
    });
    poll = setInterval(() => {
      reread();
    }, POLL_MS);
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
    if (poll !== null) clearInterval(poll);
    poll = null;
    connection.unsubscribe(subject);
    held = null;
    wire = null;
  }

  const view: Writable<SessionRead> = writable(NOTHING, () => {
    watch();
    // The subscription's own answer is the first read; this is for a store
    // that already holds one, so a page remounting on a live connection does
    // not draw nothing while the subscription is re-made.
    read(true);
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

/**
 * The session over a live connection: the record it is answered with, and the
 * read that follows every update the seat is sent.
 *
 * **Nothing is folded here, and that is the point.** A row's state, a
 * monitor's status and a server's connection state all arrive in the record
 * already decided, so an update is answered with a fresh read rather than with
 * arithmetic. This is the home's own arrangement, one subject down.
 *
 * The subject is the SEAT, and the connection routes an update to it by the
 * slot the update carries, so a store's contents are this seat's and nothing
 * else's. What arrives is therefore the whole test: no classification table is
 * needed, and none is kept.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import type { SessionSlot } from '../wire/types';
import { sessionFrom, type SessionRecord } from './wire';

/**
 * How long an update waits before it is answered with a read.
 *
 * A burst becomes one read rather than one each: a turn in flight emits
 * several frames at once, and a read answers with the session as it stands
 * rather than as it was at the first of them.
 */
const COALESCE_MS = 50;

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
export function watchSession(connection: Connection, slot: SessionSlot): Readable<SessionRead> {
  const subject = { session: slot };
  let held: Store | null = null;
  // A read is a full encode on the server - it walks the transcript and the
  // process tree - so asking again while one is in flight queues them behind
  // each other and the page falls further behind the busier the seat is.
  let reading = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      view.set({ wire: null, refused: state.why });
      return;
    }
    const data = held.snapshot();
    if (data === null) return;
    view.set({ wire: sessionFrom(data), refused: null });
  }

  function watch(): void {
    held = connection.subscribe(subject);
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
          read();
        }
        return;
      }
      if (message.kind !== 'update') return;
      if (reading || timer !== null) return;
      timer = setTimeout(() => {
        timer = null;
        reading = true;
        connection.refresh(subject);
      }, COALESCE_MS);
    });
    // A drop takes any read in flight with it, and the reconnect answers with a
    // snapshot of its own - so the pacing must not stay stuck waiting on an
    // answer that died.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') reading = false;
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
    if (timer !== null) clearTimeout(timer);
    timer = null;
    connection.unsubscribe(subject);
    held = null;
  }

  const view: Writable<SessionRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

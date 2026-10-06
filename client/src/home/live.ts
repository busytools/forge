/**
 * The home over a live connection: the snapshot it is answered with, and the
 * read that follows every update a home subscriber is sent.
 *
 * **Nothing is folded here, and that is the point.** A row's state is the
 * server's, folded from the core's own `Live` alongside the roster, so it
 * arrives in the row already decided - including the unseen marks. A client
 * that recomputed any of it from what it happens to hold would disagree with
 * the terminal the first time a turn settled while nobody was watching, and
 * it would disagree silently. So an update home is sent is answered with a
 * fresh read rather than with arithmetic, and an update it is not sent is
 * never seen.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { canHost } from '../browser/host';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { coversHome } from '../wire/fleet';
import { homeFrom, type HomeWire } from '../wire/home';

/**
 * How long an update waits before it is answered with a read.
 *
 * A burst becomes one read rather than one each: a turn settling emits
 * several at once, and a read answers with the fleet as it stands rather
 * than as it was at the first of them.
 */
const COALESCE_MS = 50;

/** What the home has to draw from, and why it has nothing when it does not. */
export interface HomeRead {
  wire: HomeWire | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: HomeRead = { wire: null, refused: null };

/**
 * Watch the home.
 *
 * The subscription, the message listener and the status listener are all let
 * go with the last subscriber, so a connection this page has replaced is not
 * left subscribed, listening, and asking the server for a full encode nobody
 * draws.
 */
export function watchHome(connection: Connection): Readable<HomeRead> {
  let held: Store | null = null;
  // A read is a full encode on the server, so asking again while one is in
  // flight queues them behind each other and the page falls further behind
  // the busier the fleet is. One at a time.
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
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards.
    view.set({ wire: data === null ? null : homeFrom(data as unknown as HomeWire), refused: null });
  }

  function watch(): void {
    // The home's subscription is also where the connection declares whether
    // this client can host the browser: it is the first subscribe the app
    // makes, and the capability belongs to the CONNECTION rather than to a
    // page - a host that only claimed it on a session page would not be the
    // host while the reader sits on the home.
    held = connection.subscribe('home', { browser: canHost() });
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        if (message.kind === 'error' || message.subject === 'home') {
          reading = false;
          read();
        }
        return;
      }
      if (message.kind !== 'update' || !coversHome(message.update)) return;
      if (reading || timer !== null) return;
      timer = setTimeout(() => {
        timer = null;
        reading = true;
        connection.refresh('home');
      }, COALESCE_MS);
    });
    // A drop takes any read in flight with it, and the reconnect answers with
    // a snapshot of its own - so the pacing must not stay stuck waiting for
    // an answer that died.
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
    // The subscription goes with the last subscriber too, or a connection
    // nothing draws from stays counted against a seat it is not showing.
    connection.unsubscribe('home');
    held = null;
  }

  const view: Writable<HomeRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

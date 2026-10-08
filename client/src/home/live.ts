/**
 * The home over a live connection: the snapshot it is answered with, and the
 * read that follows every update a home subscriber is sent.
 *
 * **Nothing is folded here, and that is the point.** A row's state is the
 * server's, folded from the core's own `Live` alongside the roster, so it
 * arrives in the row already decided - including the unseen marks. The only
 * states this client makes for itself are `stateOf`'s four promotions: the
 * cases where the state read from the wire alone would disagree with the
 * terminal, silently, the first time one of them turned. So an update the
 * home is sent is answered with a fresh read rather than with arithmetic, and
 * an update it is not sent is never seen.
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
  /**
   * Whether an update landed while a read was in flight.
   *
   * **The read in hand was encoded before it**, so its answer already
   * disagrees with the fleet: skipping the update with it leaves the page
   * drawing the state it asked about until something else moves - an ask mark
   * outliving the answer that resolved it, which is the wait Ved met leaving
   * needs-you (#1885). So the update is remembered, and asked for again the
   * moment the answer lands.
   */
  let stale = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  /** Ask for one read, coalescing the updates that arrive inside the beat. */
  function schedule(): void {
    if (timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      reading = true;
      connection.refresh('home');
    }, COALESCE_MS);
  }

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
    // The home's subscription is where the connection declares what belongs to
    // it rather than to a page: it is usually the first subscribe the app
    // makes (/models gets there first on a deep link; a seat page cannot, it
    // waits on the home's own snapshot), and both capabilities below are per
    // CONNECTION - a host or an answerer that only claimed the role on a
    // session page would not hold it while the reader sits on the home.
    //
    // **`answering` is a safeguard here, not a cure.** The core cancels an ask
    // at birth when NO subscriber can answer it anywhere, and on this machine
    // that cannot happen: forge's own terminal is attached as an answerer for
    // as long as it runs. A client that says nothing is still a guest beside
    // that. What makes an unshown seat wear its glyph is the ask itself, read
    // in the client's own `stateOf`; this declaration is for the install with
    // no terminal at all, where nobody else could answer and an ask raised
    // while this page is away would be cancelled unseen.
    held = connection.subscribe('home', { answering: true, browser: canHost() });
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        if (message.kind === 'error' || message.subject === 'home') {
          reading = false;
          read();
          // The answer just landed was encoded before whatever arrived while
          // it was in flight, so that update is spent here rather than lost.
          if (stale) {
            stale = false;
            schedule();
          }
        }
        return;
      }
      if (message.kind !== 'update' || !coversHome(message.update)) return;
      if (reading) {
        stale = true;
        return;
      }
      schedule();
    });
    // A drop takes any read in flight with it, and the reconnect answers with
    // a snapshot of its own - so the pacing must not stay stuck waiting for
    // an answer that died, and the staleness dies with it.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        reading = false;
        stale = false;
      }
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

/**
 * The home over a live connection: the snapshot it is answered with, and the
 * read that follows every update a home subscriber is sent.
 *
 * **Nothing is folded here, and that is the point.** A row's state is the
 * server's, folded from the core's own `Live` alongside the roster, so it
 * arrives in the row already decided - including the unseen marks. The only
 * states this client makes for itself are `stateOf`'s promotions: the cases
 * where the state read from the wire alone would disagree with the view this
 * client replaced, silently, the first time one of them turned. So an
 * update the home is sent is answered with a fresh read rather than with
 * arithmetic, and an update it is not sent is never seen.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { canHost } from '../browser/host';
import { subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { coversHome, serviceReport, type ServiceReport } from '../wire/fleet';
import { homeFrom, tasksChanged, type HomeWire } from '../wire/home';

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
  /**
   * Why the snapshot could not be read, in this client's words, or `null`.
   *
   * **A malformed read is its own state, not an empty home**: the store is
   * already `ready` and its bound cleared before the page narrows it, so a
   * healthy-empty drawing would stand forever with nothing saying why. The
   * page draws this instead and the next read tries again.
   */
  failed: string | null;
  /**
   * The core's last service report, or `null`.
   *
   * **It is carried rather than read off the snapshot**, because the reports
   * it passes on are moments the snapshot never holds: a refused board edit
   * says here why it did not land, and a page that only re-read on the
   * update would drop the words. The status-page issue the core HOLDS is the
   * snapshot's own `service_status`, which the home's banner draws.
   */
  report: ServiceReport | null;
}

const NOTHING: HomeRead = { wire: null, refused: null, failed: null, report: null };

/**
 * The collections a v9 home must carry. **Absent is not the older-server
 * shape here**: `homeFrom` already defaults what a v8 server omits (`fleet`,
 * a row's `misses`), so a missing `agents`, `projects`, `accounts.loading`
 * or `dictate.snapshot.models` is a read this client cannot draw as healthy.
 */
function malformedOf(data: unknown): string | null {
  if (data === null || typeof data !== 'object') return 'the snapshot was not an object';
  const held = data as Record<string, unknown>;
  const missing: string[] = [];
  if (!Array.isArray(held['agents'])) missing.push('agents');
  if (!Array.isArray(held['projects'])) missing.push('projects');
  if (held['fleet'] !== undefined && !Array.isArray(held['fleet'])) missing.push('fleet');
  const accounts = held['accounts'] as { loading?: unknown } | undefined;
  if (!Array.isArray(accounts?.loading)) missing.push('accounts');
  const dictate = held['dictate'] as { snapshot?: { models?: unknown } } | undefined;
  if (!Array.isArray(dictate?.snapshot?.models)) missing.push('dictate');
  return missing.length === 0 ? null : `the snapshot was missing ${missing.join(', ')}`;
}

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
  /** The core's last service report, read off the update that carried it. */
  let report: ServiceReport | null = null;
  /** The tasks each project last announced, so an unchanged set is no read. */
  const lastTasks = new Map<string, string>();
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
      view.set({ wire: null, refused: state.why, failed: null, report });
      return;
    }
    const data = held.snapshot();
    const malformed = data === null ? null : malformedOf(data);
    if (malformed !== null) {
      view.set({ wire: null, refused: null, failed: malformed, report });
      return;
    }
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards.
    view.set({
      wire: data === null ? null : homeFrom(data as unknown as HomeWire),
      refused: null,
      failed: null,
      report,
    });
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
    // at birth when NO subscriber can answer it anywhere, and a client that
    // says nothing is a guest while another subscriber can answer. What makes
    // an unshown seat wear its glyph is the ask itself, read in the client's
    // own `stateOf`; this declaration is for the install with no other
    // answerer, where an ask raised while this page is away would be cancelled
    // unseen.
    held = connection.subscribe('home', { answering: true, browser: canHost() });
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        // A refusal this side attributed to another subject says nothing
        // about the home's read; one off the wire names no subject and is
        // read against the home's own store.
        if (
          message.kind === 'error' &&
          message.subject !== undefined &&
          subjectKey(message.subject) !== 'home'
        ) {
          return;
        }
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
      // The line is kept BEFORE the read is asked for: the read that follows
      // is what publishes it, and a report dropped here would leave the page
      // re-reading with nothing to say why.
      const said = serviceReport(message.update);
      if (said !== null) report = said;
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // The chase sweep re-announces every project every 30 s with the set
        // unchanged; a full home re-read for that is a loop with no news.
        if (lastTasks.get(tasks.project) === tasks.digest) return;
        lastTasks.set(tasks.project, tasks.digest);
      }
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

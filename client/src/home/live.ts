/**
 * The home over a live connection: the snapshot it is answered with, the
 * read that follows an update the snapshot can carry, and the one fold.
 *
 * **Nothing is recomputed here, and that is the point.** A row's state is the
 * server's, folded from the core's own `Live` alongside the roster, so it
 * arrives in the row already decided - including the unseen marks. The only
 * states this client makes for itself are `stateOf`'s promotions: the cases
 * where the state read from the wire alone would disagree with the view this
 * client replaced, silently, the first time one of them turned. So an
 * update whose change the snapshot can carry is answered with a fresh read
 * rather than with arithmetic, and one it cannot carry is no read at all.
 *
 * `accounts_changed` is the exception, and it is a replace rather than a
 * fold: the frame carries the whole pool field, so the client draws what the
 * server states without asking for an encode - **when the move is usage
 * only.** The loading state, the settled flag, the listener and a probe
 * failure all feed the project rows' `would_bind` and chip, so a change in
 * any of those still asks for the snapshot that refreshes them.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { canHost } from '../browser/host';
import { subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { coversHome, serviceReport, type ServiceReport } from '../wire/fleet';
import {
  accountsPayload,
  homeFrom,
  poolFactsChanged,
  tasksChanged,
  type AccountsWire,
  type HomeWire,
} from '../wire/home';

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
   * page draws this instead. Nothing periodic retries it: the next update
   * that covers the home, or a reconnect, reads again.
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
 * The collections a v9 home must carry, and the shapes the renderer reads
 * out of them. **Absent is not the older-server shape here**: `homeFrom`
 * defaults the fields an older server omits (a row's `misses`, a page's
 * `unseen`), so a missing or wrong collection is a read this client cannot
 * draw as healthy. The element checks are the ones the fleet rows
 * dereference: a project's `sessions` and a seat's `slot`.
 */
function malformedOf(data: unknown): string | null {
  if (data === null || typeof data !== 'object') return 'the snapshot was not an object';
  const held = data as Record<string, unknown>;
  const wrong: string[] = [];
  const problem = (value: unknown): string | null =>
    Array.isArray(value) ? null : value === undefined ? 'was missing' : 'was not a list';
  for (const field of ['agents', 'projects', 'unseen']) {
    const why = problem(held[field]);
    if (why !== null) wrong.push(`${field} ${why}`);
  }
  if (held['fleet'] !== undefined) {
    const why = problem(held['fleet']);
    if (why !== null) wrong.push(`fleet ${why}`);
  }
  const accounts = held['accounts'] as { loading?: unknown } | undefined;
  const accountsWhy = problem(accounts?.loading);
  if (accountsWhy !== null) wrong.push(`accounts.loading ${accountsWhy}`);
  const dictate = held['dictate'] as { snapshot?: { models?: unknown } } | undefined;
  const dictateWhy = problem(dictate?.snapshot?.models);
  if (dictateWhy !== null) wrong.push(`dictate.snapshot.models ${dictateWhy}`);
  if (Array.isArray(held['projects'])) {
    const broken = held['projects'].some((entry) => {
      const project = (entry as { project?: { sessions?: unknown } | null } | null)?.project;
      return project === undefined || project === null || !Array.isArray(project.sessions);
    });
    if (broken) wrong.push('projects[].project.sessions was not a list');
  }
  if (Array.isArray(held['agents'])) {
    const broken = held['agents'].some((entry) => {
      const slot = (entry as { slot?: unknown } | null)?.slot;
      return slot === undefined || slot === null;
    });
    if (broken) wrong.push('agents[].slot was missing');
  }
  return wrong.length === 0 ? null : wrong.join('; ');
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
  /**
   * The newest pool a frame carried, kept across publishes.
   *
   * **A reused snapshot is not a newer pool.** An unsubjected `error` frame
   * re-draws what the store holds, and an in-flight read's answer was encoded
   * before the frame; either would put the last snapshot's pool back on the
   * page. So the folded pool rides every publish until a FRESH home snapshot
   * lands.
   */
  let folded: AccountsWire | null = null;
  /** Whether that fold arrived while a read (or the first snapshot) was owed. */
  let foldedDuringRead = false;
  /** The pool the drawn view holds, which a fresh frame is compared against. */
  let drawnPool: AccountsWire | null = null;
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

  /**
   * Publish the held snapshot, with `pool` replacing its accounts when a
   * frame carried one.
   *
   * **One draw for both paths** - a snapshot's own field and a frame's
   * payload - so every state (refused, empty, malformed, failed) words the
   * same way whichever arrived.
   */
  function read(pool: AccountsWire | null = null): void {
    if (held === null) return;
    drawnPool = null;
    const state = held.state();
    if (state.kind === 'refused') {
      view.set({ wire: null, refused: state.why, failed: null, report });
      return;
    }
    const data = held.snapshot();
    // A store that says `ready` and holds no snapshot is a read that lost its
    // answer, not a read still owed: drawing it as loading would wait forever.
    if (data === null) {
      const failed = state.kind === 'ready' ? 'the snapshot was empty' : null;
      view.set({ wire: null, refused: null, failed, report });
      return;
    }
    const malformed = malformedOf(data);
    if (malformed !== null) {
      view.set({ wire: null, refused: null, failed: malformed, report });
      return;
    }
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards. The guard above
    // covers the collections; the catch covers an element shape neither saw.
    try {
      const wire = homeFrom(data as unknown as HomeWire);
      if (pool !== null) wire.accounts = pool;
      drawnPool = wire.accounts;
      view.set({ wire, refused: null, failed: null, report });
    } catch (why) {
      view.set({
        wire: null,
        refused: null,
        failed: `the snapshot could not be read: ${why instanceof Error ? why.message : String(why)}`,
        report,
      });
    }
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
          if (message.kind === 'snapshot' && message.subject === 'home') {
            // A fresh snapshot states the pool itself. A fold that arrived
            // while this read was in flight is newer than the answer, so it
            // stays; any older fold goes.
            if (!foldedDuringRead) folded = null;
            foldedDuringRead = false;
          }
          // An error re-draws what the store holds, and that pool may be a
          // snapshot older than the fold.
          read(folded);
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
      // **The pool's frame carries the pool.** It is the whole field the read
      // would answer with, so folding it spends no encode; a frame from an
      // older server carries none, and falls through to the read it always
      // asked for.
      const pool = accountsPayload(message.update);
      if (pool !== null) {
        // **Usage-only moves fold; anything a project row or the card is read
        // from keeps the read.** `would_bind` and the row's chip come off the
        // pool's loading state and probe failures, so a bail, a heal or a
        // cooling has to come back with a snapshot.
        if (drawnPool !== null && poolFactsChanged(drawnPool, pool)) {
          // Falls through to the read this update asks for.
        } else {
          folded = pool;
          if (reading || held?.snapshot() === null) foldedDuringRead = true;
          else read(pool);
          return;
        }
      }
      // The line is kept BEFORE the read is asked for: the read that follows
      // is what publishes it, and a report dropped here would leave the page
      // re-reading with nothing to say why.
      const said = serviceReport(message.update);
      if (said !== null) report = said;
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // An unchanged set is no news: the chase used to re-announce every
        // project every 30 s, and a full home read for that was the loop
        // #2540 removed.
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
        // No read is owed across the drop, so the next snapshot is fresh and
        // replaces the fold; the page keeps drawing the fold until it lands.
        foldedDuringRead = false;
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

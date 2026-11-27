/**
 * The board over a live connection: the whole record its subscribe answer
 * carries, and every patch the socket applies into it after that.
 *
 * **One store per connection, shared by the pages that draw it.** The board
 * page and the session rail both read it, and a second subscription would
 * make the server encode the same rows twice. The subscription opens with the
 * first reader and goes with the last, so a home page that draws no board
 * pays nothing for it.
 *
 * **The page has no beat.** What the server computes at read time and the
 * clock moves - worked time, ages, the overdue/no-movement/waiting marks - is
 * re-derived at draw time from the row's own timestamps (`board/clock.ts`),
 * so a watched board advances without a read and nothing is sent for a
 * subject that did not change.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import {
  applyConnectors,
  applyCrons,
  applyTasks,
  boardFrom,
  connectorsFrame,
  coversBoard,
  cronsFrame,
  tasksFrame,
  type BoardRead,
  type BoardWire,
} from '../wire/board';
import { tasksChanged } from '../wire/home';

const NOTHING: BoardRead = { wire: null, refused: null };

const STORES = new WeakMap<Connection, Readable<BoardRead>>();

/**
 * The board's own reread for a connection, where a page is watching it.
 *
 * **A return's ask goes through the page's guards**, not around them: a read
 * asked while one is in flight would queue a second whole encode behind the
 * first.
 */
const REREADS = new WeakMap<Connection, () => void>();

/** Ask the board for one read through the page's own guards, if it is held. */
export function rereadBoard(connection: Connection): void {
  REREADS.get(connection)?.();
}

/** Watch the board, sharing one store with every other reader of it. */
export function watchBoard(connection: Connection): Readable<BoardRead> {
  const held = STORES.get(connection);
  if (held !== undefined) return held;
  const view = boardStore(connection);
  STORES.set(connection, view);
  return view;
}

function boardStore(connection: Connection): Readable<BoardRead> {
  let held: Store | null = null;
  /** Whether a return's read is in flight, so a second one waits for it. */
  let reading = false;
  /** The tasks each project last announced, so an unchanged set is no redraw. */
  const lastTasks = new Map<string, string>();
  /** The last board draw, so a frame's own facts can be applied into it. */
  let drawn: BoardWire | null = null;
  /**
   * What the last draw was made from, so the store's own notifications -
   * which fire for a pushed frame as much as for a new record - redraw only
   * when the record or the store's state actually moved.
   */
  let seenSnapshot: unknown = null;
  let seenVersion: number | null = null;
  let seenState: string | null = null;
  let seenHeld = 0;
  let stopStore: (() => void) | null = null;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;
  /** The instant the drawn record was received; the clock's own base. */
  let readAt = 0;
  /** The record those values were last taken from. */
  let drawnSnapshot: unknown = null;
  let drawnVersion: number | null = null;

  /**
   * Draw the store's record.
   *
   * **The store is the page's whole source**: the socket applies every patch
   * to it in place, so this reads whatever the record is now, with no ask and
   * nothing to race.
   */
  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      // Nothing to apply into: a frame that landed before the refusal must
      // not paint the refused subject back (#3074 review).
      drawn = null;
      view.set({
        wire: null,
        refused: state.why,
        readAt,
        resyncing: false,
        heldPatches: held.unexpected(),
      });
      return;
    }
    const data = held.snapshot();
    const version = held.version();
    // **The clock is the record's, not the store state's.** A version gap
    // moves the state without a new record, and stamping the receive time
    // there would undercount a running row's worked time by the gap.
    if (data !== drawnSnapshot || version !== drawnVersion) {
      drawnSnapshot = data;
      drawnVersion = version;
      readAt = Date.now();
    }
    const record = data === null ? null : boardFrom(data as unknown as BoardWire);
    drawn = record;
    view.set({
      wire: drawn,
      refused: null,
      // **A malformed record is a failure, not an empty board**: `null` from
      // the narrowing is this client's own state, and the page draws it.
      failed: data !== null && record === null ? 'the board record could not be read' : null,
      readAt,
      resyncing: state.kind === 'resyncing',
      heldPatches: held.unexpected(),
    });
  }

  /**
   * The view a frame's fold is written with.
   *
   * **A frame carries one project's facts and the board's clock stays where
   * the last whole read put it.** The derived figures a fold leaves in place
   * were computed at that read, so re-stamping the clock here would
   * re-anchor every other project's worked time to this frame. The held
   * patches and the catching-up state ride every write for the same reason
   * the wire does: `view.set` replaces the value whole, and a fold that names
   * only the wire drops both from the page.
   */
  function folded(): BoardRead {
    return {
      wire: drawn,
      refused: null,
      failed: null,
      readAt,
      resyncing: held?.state().kind === 'resyncing',
      heldPatches: held?.unexpected() ?? [],
    };
  }

  function watch(): void {
    held = connection.subscribe('board');
    // **A return's read goes through these guards.** One at a time: a second
    // ask while the first is in flight would queue a whole encode behind one
    // already on its way, and the answer lands through the store subscription
    // below, which is what clears the guard.
    REREADS.set(connection, () => {
      if (reading || held?.state().kind === 'refused') return;
      reading = true;
      if (!connection.refresh('board')) reading = false;
    });
    stopStore = held.value.subscribe(() => {
      if (held === null) return;
      const snapshot = held.snapshot();
      const version = held.version();
      const state = held.state().kind;
      // **A held patch redraws too**: the list is how the page says a frame
      // it could not place was kept rather than dropped (rule 25).
      const heldPatches = held.unexpected().length;
      if (
        snapshot === seenSnapshot &&
        version === seenVersion &&
        state === seenState &&
        heldPatches === seenHeld
      ) {
        return;
      }
      // A record that moved is the answer a return's ask was waiting on.
      if (version !== seenVersion) reading = false;
      seenSnapshot = snapshot;
      seenVersion = version;
      seenState = state;
      seenHeld = heldPatches;
      read();
    });
    stopMessages = connection.onMessage((message) => {
      if (message.kind !== 'update' || !coversBoard(message.update)) return;
      // **A refused subject draws its refusal, and the news is not lost**:
      // nothing may paint over the refusal, while the whole read that
      // re-establishes the subject spends what arrived here (#3074 review).
      if (held?.state().kind === 'refused') return;
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // An unchanged set is no news: the chase used to re-announce every
        // project every 30 s.
        if (lastTasks.get(tasks.project) === tasks.digest) return;
        lastTasks.set(tasks.project, tasks.digest);
        // **The frame carries the project's whole set**, so a task write is
        // applied into the drawn board at once; the patch that follows it
        // carries the server's own derivation (#3063, rule 32).
        const frame = tasksFrame(message.update);
        const applied =
          frame === null ? null : applyTasks(drawn, frame.project, frame.tasks, Date.now());
        if (applied !== null) {
          drawn = applied;
          view.set(folded());
        }
        return;
      }
      const crons = cronsFrame(message.update);
      if (crons !== null) {
        const applied = applyCrons(drawn, crons.project, crons.crons);
        if (applied !== null) {
          drawn = applied;
          view.set(folded());
        }
        return;
      }
      if (Object.keys(message.update).includes('connector_subscriptions_changed')) {
        // **The frame carries both sets whole**, so a project the drawn board
        // holds draws them without a read.
        const connectors = connectorsFrame(message.update);
        const applied =
          connectors === null
            ? null
            : applyConnectors(drawn, connectors.project, connectors.gotify, connectors.slack);
        if (applied !== null) {
          drawn = applied;
          view.set(folded());
        }
      }
      // **A frame this page cannot fold is not lost**: it marked the board
      // dirty on the server, and the patch that answers it redraws the store
      // within the coalescing window. There is no read to ask for.
    });
    // A drop leaves the store's record in place - the page keeps drawing what
    // it last read while the reconnect re-subscribes; the answer re-seeds the
    // store and the subscription above redraws it.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') drawn = null;
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopStore?.();
    stopMessages = null;
    stopStatus = null;
    stopStore = null;
    connection.unsubscribe('board');
    REREADS.delete(connection);
    held = null;
    drawn = null;
  }

  const view: Writable<BoardRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

/**
 * The board over a live connection: the snapshot it is answered with, and the
 * read that follows every update a board subscriber is sent (#2517).
 *
 * **One store per connection, shared by the pages that draw it.** The board
 * page and the session rail both read it, and a second subscription would
 * make the server encode the same rows twice. The subscription opens with the
 * first reader and goes with the last, so a home page that draws no board
 * pays nothing for it.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { boardFrom, coversBoard, type BoardRead, type BoardWire } from '../wire/board';
import { tasksChanged } from '../wire/home';

/** How long an update waits before it is answered with a read. */
const COALESCE_MS = 50;

/**
 * How often a mounted board re-reads while nothing writes.
 *
 * **The rows carry time-derived marks** - `updated_secs_ago`, `worked_secs`,
 * and the overdue / no-movement / waiting-too-long marks read off them - and
 * those only advance on a read. A slow beat keeps them moving without a frame
 * to trigger one (#2517).
 */
const BEAT_MS = 30_000;

const NOTHING: BoardRead = { wire: null, refused: null };

const STORES = new WeakMap<Connection, Readable<BoardRead>>();

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
  // One read at a time: it is a whole encode on the server, and a read asked
  // while one is in flight queues behind it.
  let reading = false;
  /** Whether an update landed while a read was in flight. */
  let stale = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let beat: ReturnType<typeof setInterval> | null = null;
  /** The tasks each project last announced, so an unchanged set is no read. */
  const lastTasks = new Map<string, string>();
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  function schedule(): void {
    if (timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      // A beat (or any read) already in flight is the ask this update would
      // have made: asking again would queue a second whole encode behind it.
      // **A refused subject is not asked again either** - the refusal is the
      // answer, and an update would knock on a door already shut.
      if (reading || held?.state().kind === 'refused') return;
      reading = true;
      connection.refresh('board');
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
    view.set({
      wire: data === null ? null : boardFrom(data as unknown as BoardWire),
      refused: null,
    });
  }

  function watch(): void {
    held = connection.subscribe('board');
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        // A refusal this side attributed to another subject says nothing about
        // the board's read; one off the wire names no subject and is read
        // against the board's own store.
        if (
          message.kind === 'error' &&
          message.subject !== undefined &&
          subjectKey(message.subject) !== 'board'
        ) {
          return;
        }
        if (message.kind === 'error' || message.subject === 'board') {
          reading = false;
          read();
          if (stale) {
            stale = false;
            schedule();
          }
        }
        return;
      }
      if (message.kind !== 'update' || !coversBoard(message.update)) return;
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // The chase sweep re-announces every project every 30 s with the set
        // unchanged; a full board re-read for that is a loop with no news.
        if (lastTasks.get(tasks.project) === tasks.digest) return;
        lastTasks.set(tasks.project, tasks.digest);
      }
      if (reading) {
        stale = true;
        return;
      }
      schedule();
    });
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        reading = false;
        stale = false;
      }
    });
    // The slow beat a mounted board needs: a row's ages and its time-derived
    // marks move with the clock, and no frame announces that. **A refused
    // subject is not asked again** - the refusal is the answer, and a beat
    // that re-asked it would knock on a door already shut.
    beat = setInterval(() => {
      if (reading || held?.state().kind === 'refused') return;
      reading = true;
      connection.refresh('board');
    }, BEAT_MS);
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (beat !== null) clearInterval(beat);
    beat = null;
    connection.unsubscribe('board');
    held = null;
  }

  const view: Writable<BoardRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

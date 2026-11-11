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

import { observeAttention, pageWatched } from '../attention';
import { subjectKey } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { boardFrom, coversBoard, type BoardRead, type BoardWire } from '../wire/board';
import { tasksChanged } from '../wire/home';

/**
 * How long an update waits before it is answered with a read.
 *
 * Exported because `board/live.test.ts` drives this beat on fake timers: the
 * tests wait on the beat, so a longer one must lengthen the drive rather than
 * leave the tests asserting before it fires.
 */
export const COALESCE_MS = 50;

/**
 * How often a watched board re-reads while nothing writes.
 *
 * **The beat exists for the server-derived marks alone.** `overdue`,
 * `no_movement` and `waiting_too_long` are computed at read time from the
 * rows' stamps, and no frame announces that a window crossed. Their windows
 * are hours, so a flip arriving up to five minutes late is not observable in
 * use - while the ages on screen tick off the page's own clock and need no
 * read at all (#2517).
 *
 * **And it runs only while the page is being watched.** The whole snapshot is
 * the client's largest read, and a locked screen or a display going off
 * leaves nobody to read the answer; losing attention releases the beat, and
 * gaining it takes one read and starts it again.
 */
const MARK_BEAT_MS = 5 * 60_000;

const NOTHING: BoardRead = { wire: null, refused: null };

const STORES = new WeakMap<Connection, Readable<BoardRead>>();

/**
 * The board's own reread for a connection, where a page is watching it.
 *
 * **A reader's ask goes through the page's guards**, not around them: a
 * return while a read is in flight would otherwise queue a second whole
 * encode behind the first.
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
  // One read at a time: it is a whole encode on the server, and a read asked
  // while one is in flight queues behind it.
  let reading = false;
  /** Whether an update landed while a read was in flight. */
  let stale = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let beat: ReturnType<typeof setInterval> | null = null;
  /** Whether the page had the person's attention when it was last asked. */
  let watching = false;
  let stopAttention: (() => void) | null = null;
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

  /**
   * One read through this page's guards: nothing while one is in flight, and
   * nothing from a refused subject. The beat and a return both come here.
   */
  function reread(): void {
    if (held === null || reading || held.state().kind === 'refused') return;
    reading = true;
    connection.refresh('board');
  }

  /** Start the marks' beat, if it is not already running. */
  function beatOn(): void {
    if (beat !== null) return;
    beat = setInterval(reread, MARK_BEAT_MS);
  }

  /** Release the beat: a page nobody watches has no marks to advance. */
  function beatOff(): void {
    if (beat === null) return;
    clearInterval(beat);
    beat = null;
  }

  /**
   * The page's attention moved.
   *
   * **A return is one read and a restarted beat**: the marks moved while
   * nothing was watching, and that read is what they stand at now. It goes
   * through the page's guards like every other read, so one already in
   * flight is not asked twice.
   */
  function attention(next: boolean): void {
    // **One chain per watched period.** The repeated reports a comeback can
    // produce must not each buy a whole-board encode.
    if (next === watching) return;
    watching = next;
    if (!next) {
      beatOff();
      return;
    }
    reread();
    beatOn();
  }

  function watch(): void {
    held = connection.subscribe('board');
    reading = false;
    // **Registered with the watch, not the store**, so a re-watch after the
    // last reader left registers it again.
    REREADS.set(connection, reread);
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
        // An unchanged set is no news: the chase used to re-announce every
        // project every 30 s, and a full board read for that was the loop
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
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') {
        reading = false;
        stale = false;
      }
    });
    // The slow beat the server-derived marks need, while there is a page
    // being watched to read it: **a refused subject is not asked again** -
    // the refusal is the answer, and a beat that re-asked it would knock on a
    // door already shut.
    watching = pageWatched();
    if (watching) beatOn();
    stopAttention = observeAttention(attention);
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopAttention?.();
    stopMessages = null;
    stopStatus = null;
    stopAttention = null;
    if (timer !== null) clearTimeout(timer);
    timer = null;
    beatOff();
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

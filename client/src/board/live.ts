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
import { observeForeground } from '../foreground';
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
 * **The beat exists for what the server computes at read time.** The marks
 * `no_movement` and `waiting_too_long` are four-hour windows and `overdue`
 * follows the row's own estimate, and no frame announces that one crossed.
 * The card's worked figure and the waiting card's age are read values too, so
 * they are refreshed at most five minutes after the page is watched again and
 * can be up to the time it was away plus five minutes old - an accepted
 * staleness, because these windows are hours or an estimate (#2517). Only the
 * `wrote ... ago` line ticks off the page's own clock and needs no read.
 *
 * **And it runs only while the page is being watched.** The whole snapshot is
 * one of the client's largest reads, and a locked screen or a display going
 * off leaves nobody to read the answer; losing attention releases the beat,
 * and gaining it starts it again - from focus or visibility on the desktop and
 * the web, and from the shell's `client://foreground` resume on Android, which
 * reports neither on return.
 *
 * **Gaining attention does not read.** The return's read already exists:
 * `connect/resync.ts` asks the board through this store's own guards after a
 * hide of at least `RETURN_HIDE_MS`, and reads nothing on a bare focus change
 * on purpose. A read here would buy one on every alt-tab, and a second on
 * every return from hidden.
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
  let stopAttention: (() => void) | null = null;
  let stopForeground: (() => void) | null = null;
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

  /**
   * Start the marks' beat, if it is not already running.
   *
   * **A report can land after the last reader left** - the shell's resume is
   * a promise, and its disconnect may not have completed - so nothing is
   * armed for a store nobody holds: an interval left behind would be a timer
   * nothing ever clears.
   */
  function beatOn(): void {
    if (beat !== null || held === null) return;
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
   * **A return restarts the beat and reads nothing of its own.** The return's
   * read already exists in `connect/resync.ts`, which asks this store through
   * its own guards after a hide of at least `RETURN_HIDE_MS` and deliberately
   * reads nothing on a bare focus change; a read here would buy one on every
   * alt-tab, and a second on every return from hidden.
   *
   * Both calls are idempotent, so a report that changes nothing - a restored
   * page, a repeated focus - costs nothing.
   */
  function attention(next: boolean): void {
    if (next) beatOn();
    else beatOff();
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
    // door already shut. **The shell's resume is its own arm**: Android
    // reports neither focus nor reliably a visibility change on return, so
    // without it the beat would never come back after the first suspend.
    if (pageWatched()) beatOn();
    stopAttention = observeAttention(attention);
    stopForeground = observeForeground('board marks beat', () => attention(true));
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopAttention?.();
    stopForeground?.();
    stopMessages = null;
    stopStatus = null;
    stopAttention = null;
    stopForeground = null;
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

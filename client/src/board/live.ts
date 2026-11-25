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
import { ReadPace } from '../read-pace';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import {
  applyCrons,
  applyTasks,
  boardFrom,
  coversBoard,
  cronsFrame,
  tasksFrame,
  type BoardRead,
  type BoardWire,
} from '../wire/board';
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
  /**
   * The pace every frame-driven read is asked at, floored so a fleet whose
   * news never stops cannot buy a whole encode per frame (#3060).
   */
  const pace = new ReadPace(() => {
    // A read already in flight is the ask this update would have made:
    // asking again would queue a second whole encode behind it.
    // **A refused subject is not asked again either** - the refusal is the
    // answer, and an update would knock on a door already shut.
    if (reading || held?.state().kind === 'refused') return false;
    reading = true;
    return connection.refresh('board');
  }, COALESCE_MS);
  let beat: ReturnType<typeof setInterval> | null = null;
  let stopAttention: (() => void) | null = null;
  let stopForeground: (() => void) | null = null;
  /** The tasks each project last announced, so an unchanged set is no read. */
  const lastTasks = new Map<string, string>();
  /** The last board draw, so a frame's own facts can be applied into it. */
  let drawn: BoardWire | null = null;
  /** A frame applied while the read in hand was being encoded (#3074). */
  let appliedDuringRead = false;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  function schedule(): void {
    pace.schedule();
  }

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      // Nothing to apply into: a frame that landed before the refusal must
      // not paint the refused subject back (#3074 review).
      drawn = null;
      view.set({ wire: null, refused: state.why });
      return;
    }
    const data = held.snapshot();
    drawn = data === null ? null : boardFrom(data as unknown as BoardWire);
    view.set({ wire: drawn, refused: null });
  }

  /**
   * One read through this page's guards: nothing while one is in flight, and
   * nothing from a refused subject. The marks' beat and a return ask here
   * directly rather than through the pace, and the ask starts the floor so
   * the pace cannot buy a second whole encode right behind it.
   */
  function reread(): void {
    if (held === null || reading || held.state().kind === 'refused') return;
    reading = true;
    if (connection.refresh('board')) pace.went();
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
          if (appliedDuringRead) {
            // **The answer was encoded before the frame already drawn**: it
            // must not overwrite that frame's news. The ask goes again, and
            // the next answer is the one newer than the frame.
            appliedDuringRead = false;
            stale = false;
            schedule();
            return;
          }
          read();
          if (stale) {
            stale = false;
            schedule();
          }
        }
        return;
      }
      if (message.kind !== 'update' || !coversBoard(message.update)) return;
      // **A refused subject draws its refusal, and the news is not lost**:
      // nothing may paint over the refusal, but the update is spent by the
      // snapshot that re-establishes the subject (#3074 review).
      if (held?.state().kind === 'refused') {
        stale = true;
        return;
      }
      const tasks = tasksChanged(message.update);
      if (tasks !== null) {
        // An unchanged set is no news: the chase used to re-announce every
        // project every 30 s, and a full board read for that was the loop
        // #2540 removed.
        if (lastTasks.get(tasks.project) === tasks.digest) return;
        lastTasks.set(tasks.project, tasks.digest);
        // **The frame carries the project's whole set**, so a task write is
        // applied into the drawn board - the read it replaces was 3 MB and ran
        // at 0.39/s while a seat was on screen (#3063, rule 32).
        const frame = tasksFrame(message.update);
        const applied = frame === null ? null : applyTasks(drawn, frame.project, frame.tasks);
        if (applied !== null) {
          drawn = applied;
          view.set({ wire: drawn, refused: null });
          // **A read already in flight was encoded before this frame and will
          // land after it**: the landing is dropped and the ask goes again
          // rather than overwriting the frame's news.
          if (reading) appliedDuringRead = true;
          return;
        }
      } else {
        const crons = cronsFrame(message.update);
        if (crons !== null) {
          const applied = applyCrons(drawn, crons.project, crons.crons);
          if (applied !== null) {
            drawn = applied;
            view.set({ wire: drawn, refused: null });
            if (reading) appliedDuringRead = true;
            return;
          }
        } else if (Object.keys(message.update).includes('connector_subscriptions_changed')) {
          // The connector sets are still a read: their wire shape is
          // `unknown` here, so folding them is its own change. Rare - a user
          // action or a door no tool reaches, not a per-second beat.
          if (reading) {
            stale = true;
            return;
          }
          schedule();
          return;
        }
        // **The six seat-liveness variants read nothing.** A row's stale chip
        // and its ages are the server's own computation; the marks beat
        // reconciles them within five minutes, the staleness it already
        // accepts. Reading 3 MB for a chip was the loop #3063 measured.
        return;
      }
      // The first read is still owed: the frame has nowhere to land.
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
        // A reconnect re-subscribes and answers with a fresh snapshot; the
        // frames in between have nowhere to land.
        drawn = null;
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
    pace.stop();
    beatOff();
    connection.unsubscribe('board');
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

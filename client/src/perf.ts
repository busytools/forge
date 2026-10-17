/**
 * The client's own frame instrument.
 *
 * The view this client replaced wrote a sidecar frame log, and with that view
 * gone the instrument moves to the layer that actually stutters: the page. The
 * page MEASURES - rAF cadence and the long tasks the browser itself reports
 * are page facts - and hands finished records to the shell over `invoke`,
 * which is where the file lives. That is the client's own split (Ved,
 * 2026-10-12): measuring stays in the page, and only the write needs the OS
 * side.
 *
 * **The slow-frame rule from the view this client replaced is kept.** Only a
 * frame at or above [`SLOW_FRAME_MS`] is written; one record per frame would
 * drown a file nobody reads until something is wrong. The periodic summary is
 * what makes a quiet log useful - it says the page was alive and how its
 * cadence behaved - and long tasks carry their own name, so a record says
 * WHICH work blocked, not merely that something did.
 *
 * Nothing here may break the app: a missing shell (the web build) or a
 * refused invoke is silence, never an error thrown into a render path.
 */

import { invoke, isTauri } from '@tauri-apps/api/core';

import { CLIENT_VERSION } from './protocol';
import type { Route } from './routes';
import { mintPromptId } from './wire/ids';

/** The record schema, shared with the log the shell writes. */
const SCHEMA = 'forge-perf/v1';

/**
 * A frame this long is written. The same threshold the view this client
 * replaced used, kept because the lattice it was chosen against - what a
 * person notices, what a 60 Hz display budgets - has not moved.
 */
const SLOW_FRAME_MS = 50;

/** How often a window's cadence is summarised, the same cadence the view this
 *  client replaced used. */
const SUMMARY_INTERVAL_MS = 10_000;

/** One line of the perf log, as the shell writes it. */
export interface PerfRecord {
  schema: string;
  kind: 'run_started' | 'frame' | 'longtask' | 'summary' | 'switch' | 'first_row' | 'quiet';
  run_id: string;
  ts_ms: number;
  version?: string;
  /** The frame's own counter within the run, for a frame record. */
  frame?: number;
  /**
   * The frame's gap, or the long task's duration, or - for a `first_row` and
   * a `quiet` - the time since the switch record the mark belongs to.
   */
  ms?: number;
  /** A long task's own name. */
  name?: string;
  /**
   * The route the page was on, and the seat it held.
   *
   * A frame and a long task carry where they happened. A `run_started`
   * carries boot's own reading, and a `summary` the reading as of when its
   * window OPENED - a window can span a seat switch, so one value cannot
   * say where every frame inside it happened, and the frames carry their
   * own. Null is honest for "the router has not wired a route yet".
   */
  surface?: string | null;
  session?: string | null;
  window_ms?: number;
  frames?: number;
  slow?: number;
  max_ms?: number;
}

interface PerfProbe {
  start(): void;
  stop(): void;
  /** The page the probe is looking at; nulls for a route with no seat. */
  setSurface(surface: string | null, session: string | null): void;
  /** One row of the open seat's conversation has drawn (#2374). */
  noteFirstRow(): void;
}

/** The clock and sources, injectable so a test can produce a known frame. */
interface ProbeOptions {
  slowFrameMs?: number;
  summaryIntervalMs?: number;
  now?: () => number;
  /** Runs `cb` on the next frame; returns its own cancel. */
  schedule?: (cb: () => void) => () => void;
  /** Reports long tasks; returns its own disconnect. */
  observeLongTasks?: (cb: (ms: number, name: string) => void) => () => void;
  /** Where finished records go; the shell write by default. */
  sink?: (records: PerfRecord[]) => void;
  version?: string;
}

function nextFrame(cb: () => void): () => void {
  const id = requestAnimationFrame(cb);
  return () => cancelAnimationFrame(id);
}

function observeLongTasks(cb: (ms: number, name: string) => void): () => void {
  if (typeof PerformanceObserver === 'undefined') return () => {};
  if (!PerformanceObserver.supportedEntryTypes.includes('longtask')) return () => {};
  const observer = new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) cb(entry.duration, entry.name);
  });
  observer.observe({ type: 'longtask', buffered: true });
  return () => observer.disconnect();
}

/** The shell's write: one invoke, fire-and-forget. */
function sinkToShell(records: PerfRecord[]): void {
  void invoke('client_perf', { records }).catch(() => {
    // A refused write is a lost record, not a broken page: the shell may be
    // gone, and a perf sink that threw would take a render path with it.
  });
}

/**
 * The probe itself, over injected sources.
 *
 * One frame of the same shape the view this client replaced wrote comes out of
 * it: the frame counter counts every frame, the slow ones are written, and a
 * window that crossed [`SUMMARY_INTERVAL_MS`] is summarised as the frame that
 * crossed it lands (so the summary counts that frame, the same rule that view
 * applied).
 */
export function perfProbe(options: ProbeOptions = {}): PerfProbe {
  const now = options.now ?? (() => performance.now());
  const slowFrameMs = options.slowFrameMs ?? SLOW_FRAME_MS;
  const summaryIntervalMs = options.summaryIntervalMs ?? SUMMARY_INTERVAL_MS;
  const sink = options.sink ?? sinkToShell;
  const version = options.version ?? CLIENT_VERSION;
  const schedule = options.schedule ?? nextFrame;
  const observe = options.observeLongTasks ?? observeLongTasks;

  const runId = mintPromptId();
  let surface: string | null = null;
  let session: string | null = null;
  /** Where the OPEN window began, which is what its summary reports. */
  let windowSurface: string | null = null;
  let windowSession: string | null = null;
  let frames = 0;
  let slow = 0;
  let maxMs = 0;
  let windowStart = 0;
  let last: number | null = null;
  let cancelFrame: (() => void) | null = null;
  let cancelObserver: (() => void) | null = null;
  /**
   * The seat the page is showing, and whether a route has been wired yet.
   *
   * **A seat that was not already on screen is a switch** (#2374): the
   * records a reader measures a slow switch by - `first_row` and `quiet` -
   * are timed from when the seat came on screen. The first wiring is boot's
   * own read, and the run line already brackets that.
   */
  let shown: string | null = null;
  let wired = false;
  /** When the open seat came on screen, and whether its rows have drawn. */
  let openAt: number | null = null;
  let firstRowSeen = false;

  function emit(record: PerfRecord): void {
    sink([record]);
  }

  /** Close the open window: one summary line, then a fresh window under
   *  wherever the page is now. A window with no frames says nothing. */
  function flushWindow(at: number): void {
    if (frames === 0) return;
    emit({
      schema: SCHEMA,
      kind: 'summary',
      run_id: runId,
      ts_ms: Date.now(),
      window_ms: at - windowStart,
      frames,
      slow,
      max_ms: maxMs,
      surface: windowSurface,
      session: windowSession,
    });
    frames = 0;
    slow = 0;
    maxMs = 0;
    windowStart = at;
    windowSurface = surface;
    windowSession = session;
  }

  function tick(): void {
    const at = now();
    frames += 1;
    if (last !== null) {
      const gap = at - last;
      if (gap > maxMs) maxMs = gap;
      if (gap >= slowFrameMs) {
        slow += 1;
        emit({
          schema: SCHEMA,
          kind: 'frame',
          run_id: runId,
          ts_ms: Date.now(),
          frame: frames,
          ms: gap,
          surface,
          session,
        });
      } else if (openAt !== null && firstRowSeen) {
        // **The open seat's work is done**: its rows are drawn and the
        // frame loop is back to a normal cadence, which is the quiet the
        // switch's own timing is measured to.
        emit({
          schema: SCHEMA,
          kind: 'quiet',
          run_id: runId,
          ts_ms: Date.now(),
          ms: at - openAt,
          surface,
          session,
        });
        openAt = null;
      }
    }
    last = at;
    if (at - windowStart >= summaryIntervalMs) flushWindow(at);
    cancelFrame = schedule(tick);
  }

  return {
    start(): void {
      last = null;
      frames = 0;
      slow = 0;
      maxMs = 0;
      openAt = null;
      firstRowSeen = false;
      windowStart = now();
      // Boot's own reading: the router has usually not wired a route yet,
      // so null here is the honest answer rather than a gap.
      windowSurface = surface;
      windowSession = session;
      emit({
        schema: SCHEMA,
        kind: 'run_started',
        run_id: runId,
        ts_ms: Date.now(),
        version,
        surface,
        session,
      });
      cancelObserver = observe((ms, name) => {
        emit({
          schema: SCHEMA,
          kind: 'longtask',
          run_id: runId,
          ts_ms: Date.now(),
          ms,
          name,
          surface,
          session,
        });
      });
      cancelFrame = schedule(tick);
    },
    stop(): void {
      // The open window still lands, the same rule the view this client
      // replaced applied at shutdown: a partial window is what the reader
      // wants when the page is going away, and dropping it is a silence
      // nobody can act on.
      flushWindow(now());
      cancelFrame?.();
      cancelObserver?.();
      cancelFrame = null;
      cancelObserver = null;
    },
    setSurface(next: string | null, seat: string | null): void {
      surface = next;
      session = seat;
      if (wired && seat !== null && seat !== shown) {
        openAt = now();
        firstRowSeen = false;
        emit({
          schema: SCHEMA,
          kind: 'switch',
          run_id: runId,
          ts_ms: Date.now(),
          surface,
          session,
        });
      }
      shown = seat;
      wired = true;
    },
    noteFirstRow(): void {
      if (openAt === null || firstRowSeen) return;
      firstRowSeen = true;
      emit({
        schema: SCHEMA,
        kind: 'first_row',
        run_id: runId,
        ts_ms: Date.now(),
        ms: now() - openAt,
        surface,
        session,
      });
    },
  };
}

/** The running probe, once one was started. */
let running: PerfProbe | null = null;

/**
 * Start the probe, once, and only where there is a shell to write to. The
 * web build measures nothing: a record with no file behind it helps no one,
 * and a refused invoke per frame is worse.
 */
export function startPerfProbe(options: ProbeOptions = {}): PerfProbe | null {
  if (running !== null) return running;
  if (!isTauri()) return null;
  running = perfProbe(options);
  running.start();
  return running;
}

/** Where the page is, for the records the probe writes from here on. */
export function notePerfSurface(surface: string | null, session: string | null): void {
  running?.setSurface(surface, session);
}

/**
 * One row of the open seat's conversation has drawn: the mark a seat
 * switch's own timing is measured to, written where the rows land rather
 * than where the click happened (#2374).
 */
export function noteFirstRow(): void {
  running?.noteFirstRow();
}

/**
 * A route, as the probe's records name it: the surface is the route's own
 * name, and the session is the seat, when the route holds one. #1596 asks
 * that a record say which surface and which session a cost came from - this
 * is the pair that does it.
 */
export function perfSurface(route: Route): { surface: string; session: string | null } {
  if ('slot' in route) {
    return {
      surface: route.name,
      session: `${route.slot.org}/${route.slot.project}/${route.slot.label}`,
    };
  }
  if (route.name === 'board') return { surface: 'board', session: `${route.org}/${route.project}` };
  return { surface: route.name, session: null };
}

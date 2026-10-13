/**
 * The client's own frame instrument.
 *
 * The terminal wrote a sidecar frame log (`crates/forge-tui/src/perf.rs`) and
 * that crate is on its way out, so the instrument moves to the layer that
 * actually stutters: the page. The page MEASURES - rAF cadence and the long
 * tasks the browser itself reports are page facts - and hands finished
 * records to the shell over `invoke`, which is where the file lives. That is
 * the client's own split (Ved, 2026-10-12): measuring stays in the page, and
 * only the write needs the OS side.
 *
 * **The terminal's slow-frame rule is kept.** Only a frame at or above
 * [`SLOW_FRAME_MS`] is written; one record per frame would drown a file
 * nobody reads until something is wrong. The periodic summary is what makes
 * a quiet log useful - it says the page was alive and how its cadence
 * behaved - and long tasks carry their own name, so a record says WHICH work
 * blocked, not merely that something did.
 *
 * Nothing here may break the app: a missing shell (the web build) or a
 * refused invoke is silence, never an error thrown into a render path.
 */

import { invoke, isTauri } from '@tauri-apps/api/core';

import { CLIENT_VERSION } from './protocol';
import type { Route } from './routes';

/** The record schema, shared with the log the shell writes. */
const SCHEMA = 'forge-perf/v1';

/**
 * A frame this long is written. The terminal's threshold, kept because the
 * lattice it was chosen against - what a person notices, what a 60 Hz
 * display budgets - has not moved.
 */
export const SLOW_FRAME_MS = 50;

/** How often a window's cadence is summarised, the terminal's cadence. */
const SUMMARY_INTERVAL_MS = 10_000;

/** One line of the perf log, as the shell writes it. */
export interface PerfRecord {
  schema: string;
  kind: 'run_started' | 'frame' | 'longtask' | 'summary';
  run_id: string;
  ts_ms: number;
  version?: string;
  /** The frame's own counter within the run, for a frame record. */
  frame?: number;
  /** The frame's gap, or the long task's duration, in milliseconds. */
  ms?: number;
  /** A long task's own name. */
  name?: string;
  /** The route the page was on, and the seat it held. */
  surface?: string | null;
  session?: string | null;
  window_ms?: number;
  frames?: number;
  slow?: number;
  max_ms?: number;
}

export interface PerfProbe {
  start(): void;
  stop(): void;
  /** The page the probe is looking at; nulls for a route with no seat. */
  setSurface(surface: string | null, session: string | null): void;
}

/** The clock and sources, injectable so a test can produce a known frame. */
export interface ProbeOptions {
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
 * One frame of the terminal's own shape comes out of it: the frame counter
 * counts every frame, the slow ones are written, and a window that crossed
 * [`SUMMARY_INTERVAL_MS`] is summarised as the frame that crossed it lands
 * (so the summary counts that frame, the terminal's rule).
 */
export function perfProbe(options: ProbeOptions = {}): PerfProbe {
  const now = options.now ?? (() => performance.now());
  const slowFrameMs = options.slowFrameMs ?? SLOW_FRAME_MS;
  const summaryIntervalMs = options.summaryIntervalMs ?? SUMMARY_INTERVAL_MS;
  const sink = options.sink ?? sinkToShell;
  const version = options.version ?? CLIENT_VERSION;
  const schedule = options.schedule ?? nextFrame;
  const observe = options.observeLongTasks ?? observeLongTasks;

  const runId = crypto.randomUUID();
  let surface: string | null = null;
  let session: string | null = null;
  let frames = 0;
  let slow = 0;
  let maxMs = 0;
  let windowStart = 0;
  let last: number | null = null;
  let cancelFrame: (() => void) | null = null;
  let cancelObserver: (() => void) | null = null;

  function emit(record: PerfRecord): void {
    sink([record]);
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
      }
    }
    last = at;
    if (at - windowStart >= summaryIntervalMs) {
      emit({
        schema: SCHEMA,
        kind: 'summary',
        run_id: runId,
        ts_ms: Date.now(),
        window_ms: at - windowStart,
        frames,
        slow,
        max_ms: maxMs,
      });
      frames = 0;
      slow = 0;
      maxMs = 0;
      windowStart = at;
    }
    cancelFrame = schedule(tick);
  }

  return {
    start(): void {
      last = null;
      frames = 0;
      slow = 0;
      maxMs = 0;
      windowStart = now();
      emit({
        schema: SCHEMA,
        kind: 'run_started',
        run_id: runId,
        ts_ms: Date.now(),
        version,
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
      cancelFrame?.();
      cancelObserver?.();
      cancelFrame = null;
      cancelObserver = null;
    },
    setSurface(next: string | null, seat: string | null): void {
      surface = next;
      session = seat;
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

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

import { HEARTBEAT_TICK_MS } from './heartbeat';
import { CLIENT_VERSION } from './protocol';
import type { Route } from './routes';
import { mintPromptId } from './wire/ids';
import type { SessionSlot } from './wire/types';

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

/**
 * Which terminal drawing is on screen for a seat's open (#2374).
 *
 * **`conversation` and `empty` are the seat's own answer** - rows on screen,
 * or the loaded-and-empty state. `waking` and `refused` are the column
 * saying something else, and one of those does NOT end the open: the
 * content's own drawing follows it. `waking` is the umbrella for the waking
 * line and the not-running line - one state to the reader, and the four
 * words are the whole vocabulary on purpose.
 */
export type DrawnState = 'conversation' | 'empty' | 'waking' | 'refused';

/** One line of the perf log, as the shell writes it. */
export interface PerfRecord {
  schema: string;
  kind:
    'run_started' | 'frame' | 'longtask' | 'summary' | 'switch' | 'drawn' | 'quiet' | 'heartbeat';
  run_id: string;
  ts_ms: number;
  version?: string;
  /** The frame's own counter within the run, for a frame record. */
  frame?: number;
  /**
   * The frame's gap, or the long task's duration, or - for a `drawn` and a
   * `quiet` - the time since the switch record the mark belongs to.
   */
  ms?: number;
  /**
   * For a `drawn`: WHICH terminal state completed the open. Only
   * `conversation` and `empty` are the seat's own answer; a `waking` or
   * `refused` line is the column saying something else, and must never read
   * as the content arriving (#2374).
   */
  state?: DrawnState;
  /**
   * **The Web Worker's heartbeat, spanning a slow `frame`** (#2374). The
   * worker ticks once a second and the page times each arrival, so the pair
   * says whether the page's clock ran while frames did not, and when:
   *
   * - `worker_beats`: how many ticks arrived during the gap.
   * - `worker_max_late_ms`: the WORST lateness among them - a tick the page
   *   received long after it was sent. A blocked main thread queues the
   *   worker's messages and delivers them late, in a burst.
   * - `worker_since_beat_ms`: how long before this frame the last tick
   *   arrived. **Read it against the one-second tick**: a gap shorter than
   *   a tick can carry no beat at all, and `worker_beats: 0` with
   *   `worker_since_beat_ms` under the tick is "no beat was due", while a
   *   `since_beat` of several tick periods is a page that stopped ticking.
   *
   * All three are absent where no Web Worker ran (or the worker failed to
   * load), because absence must not read as zero.
   */
  worker_beats?: number;
  worker_max_late_ms?: number;
  worker_since_beat_ms?: number;
  /**
   * How many ticks the gap would have carried at the worker's interval
   * (`floor(ms / worker_tick_ms)`).
   *
   * **The reading takes BOTH axes - the count and the lateness:**
   * - `worker_beats` about equal to this AND `worker_max_late_ms` low: the
   *   page ran on its usual clock while frames stopped (a compositing or
   *   visibility gate, not a busy thread).
   * - `worker_beats` about equal to this AND `worker_max_late_ms`
   *   approaching the gap: the busy-main-thread burst - the ticks ran
   *   off-thread and the page received them on release, late by the block.
   * - `worker_beats` about one with this far above it: the suspended shape
   *   (a resumed page fires its one overdue interval).
   * - `worker_beats` zero with this at least one: nothing ticked.
   */
  worker_expected_beats?: number;
  /**
   * Whether a Web Worker heartbeat is running: on `run_started` it says the
   * page has one at all, and a `heartbeat` record with `false` says the
   * worker failed to load and the beat fields are absent from then on.
   */
  heartbeat?: boolean;
  /** How long the heartbeat's tick interval is, for the reading above. */
  worker_tick_ms?: number;
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
  /** A conversation opened for `seat`: a click, a deep link, a reload, a
   *  reconnect or a palette move, written where the column takes the seat on
   *  (#2374). */
  noteConversationOpened(seat: string): void;
  /** The open seat's terminal drawing has landed, in `state` (#2374). */
  noteDrawn(state: DrawnState): void;
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
  /** Reports each off-thread heartbeat, how late it arrived and whether the
   *  worker failed to load; returns its own disconnect, or null where the
   *  page has no Web Worker at all - which is what keeps the evidence
   *  fields absent rather than claiming zero (#2374). */
  observeHeartbeat?: (
    cb: (lateMs: number) => void,
    failed: (why: string) => void,
  ) => (() => void) | null;
  /** Where finished records go; the shell write by default. */
  sink?: (records: PerfRecord[]) => void;
  version?: string;
  /**
   * Runs `cb` after one macrotask, which is where a slow frame's record is
   * written (#2374): the worker's ticks queued DURING a blocked gap are
   * delivered as tasks right after the block, usually after the frame
   * callback that measured the gap - so the record is deferred past that
   * turn or it would report the burst's beats as a page that stopped.
   * Injectable so a test drives the turn rather than racing it.
   */
  defer?: (cb: () => void) => void;
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

/**
 * The off-thread heartbeat: one tick a second from a Web Worker, and each
 * one's lateness measured against the page's own clock.
 *
 * **A Web Worker is what makes the split possible** (#2374): its timer runs
 * on another thread, so ticks that REACH THE PAGE during a stalled frame
 * loop prove the page's main thread ran while frames did not; a busy main
 * thread queues them and delivers them late, in a burst. No ticks over
 * several tick periods means the page was not running on its usual clock -
 * not composited, App-Nap'd, or locked - and the record says which of those
 * only as far as the measured tick rate goes (see the README).
 *
 * **Both clocks are monotonic and the offset is the minimum seen**, so a
 * wall-clock step cannot read as lateness, and the transport delay of the
 * fastest tick is the baseline every other one is measured against.
 *
 * **A worker that fails to load is announced, never silent**: `new Worker`
 * does not throw for a script that cannot be fetched or throws on load -
 * those arrive as `error`/`messageerror`. This port reports that to
 * `failed`, so the fields go ABSENT rather than reading as a stopped page.
 * A page without Worker at all answers null.
 */
function observeHeartbeat(
  cb: (lateMs: number) => void,
  failed: (why: string) => void,
): (() => void) | null {
  if (typeof Worker === 'undefined') return null;
  let worker: Worker;
  try {
    worker = new Worker(new URL('./heartbeat.worker.ts', import.meta.url), { type: 'module' });
  } catch (error) {
    // **A construction throw is a FAILURE, not an absent worker**: a page
    // that could not build its worker must read differently from one that
    // has none, so this reports before answering null.
    const why = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    failed(`constructor: ${why}`);
    return null;
  }
  /** The smallest page-minus-worker time seen: the clock offset, less the
   *  fastest tick's transport delay. */
  let offset = Infinity;
  const onMessage = (event: MessageEvent): void => {
    const sent: unknown = (event.data as { t?: unknown } | null)?.t;
    if (typeof sent !== 'number') return;
    const raw = performance.now() - sent;
    if (raw < offset) offset = raw;
    cb(raw - offset);
  };
  const onFailure = (event: Event): void => {
    // **The cause lives on `event.error`, not on the event.** A worker's
    // script error is an `ErrorEvent` whose `name`/`message` sit in
    // `error`; `messageerror` is a `MessageEvent` carrying neither; a fetch
    // or MIME failure is a plain `Event` with only its type. So the record
    // names the error where there is one and the event's own type otherwise,
    // rather than reporting a name no DOM event carries.
    const inner = (event as { error?: { name?: unknown; message?: unknown } }).error;
    const name = typeof inner?.name === 'string' ? inner.name : event.type;
    const message = typeof inner?.message === 'string' ? inner.message : '';
    failed(message === '' ? name : `${name}: ${message}`);
  };
  worker.addEventListener('message', onMessage);
  worker.addEventListener('error', onFailure);
  worker.addEventListener('messageerror', onFailure);
  return () => {
    worker.removeEventListener('message', onMessage);
    worker.removeEventListener('error', onFailure);
    worker.removeEventListener('messageerror', onFailure);
    worker.terminate();
  };
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
  const heartbeat = options.observeHeartbeat ?? observeHeartbeat;
  const defer =
    options.defer ??
    ((cb: () => void): void => {
      setTimeout(cb, 0);
    });

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
  let cancelHeartbeat: (() => void) | null = null;
  /**
   * Whether the Web Worker heartbeat is live: false where the page has none,
   * or where the worker failed to load after starting. A false here is what
   * keeps the beat fields ABSENT from the records rather than reading as a
   * page that stopped (#2374).
   */
  let heartbeatOk = false;
  /**
   * Whether the one failure record has been written. **A separate flag from
   * `heartbeatOk`, because a construction throw reports BEFORE the port
   * answers**: the port's catch calls the failure callback synchronously, so
   * a guard keyed on the not-yet-set heartbeat would swallow it.
   */
  let failedSeen = false;
  /**
   * The beats since the last record, the WORST lateness among them, and when
   * the last one arrived - what a slow frame reports about whether the page
   * kept running while frames did not (#2374). They reset with a fast frame
   * that has no record waiting on them, and with every flush; the arrival
   * time carries across, because "when did the last beat land" is what tells
   * a gap shorter than a tick from a page that stopped ticking.
   */
  let beats = 0;
  let beatLate = 0;
  let lastBeatAt: number | null = null;
  /**
   * The slow frame waiting for its record, and the beats that reached the
   * page after it was measured (#2374).
   *
   * **A blocked gap's evidence arrives AFTER the frame that measured it.**
   * The worker's messages queued during the block are delivered as tasks
   * once the main thread frees - usually after the frame callback - so a
   * record written in that callback would report zeros for a thread that
   * was blocked. The record is therefore written one macrotask later, and
   * any beat delivered in between is the gap's own (it was SENT during the
   * gap): those go in `postBeats`/`postLate`.
   */
  let pending: {
    gap: number;
    frame: number;
    sinceBeat: number | null;
    beats: number;
    beatLate: number;
    surface: string | null;
    session: string | null;
  } | null = null;
  let postBeats = 0;
  let postLate = 0;
  let flushArmed = false;
  /**
   * The open conversation's own seat, when it started, and what has drawn
   * for it (#2374).
   *
   * **The marks name the seat that OPENED, not whatever the page shows
   * next**: a reader who leaves the seat before its first normal frame would
   * otherwise read the seat's own delta attributed to the home they moved
   * to. And the open is dropped the moment the page leaves that seat: a mark
   * left armed across the move would land on an unrelated moment.
   *
   * **One placeholder and one content drawing per open.** A `waking` or
   * `refused` line is the column saying something else, and it does NOT end
   * the open: when the seat's content lands after one, its own `drawn`
   * follows and only then can a `quiet` settle the open. Two flags, because
   * the column re-runs the mark on every update it holds and the log wants
   * the first of each, not a burst.
   */
  let openSeat: string | null = null;
  let openAt: number | null = null;
  let placeholderSeen = false;
  let contentSeen = false;

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

  /**
   * Write the waiting slow frame's record, with every beat that reached the
   * page since it was measured.
   *
   * **The wait is the point** (#2374): a blocked gap's queued ticks are
   * delivered as tasks after the frame callback that measured the gap, so
   * the record is written one macrotask later - by then the burst has
   * landed and `worker_beats`/`worker_max_late_ms` carry it. A beat that
   * lands between the measurement and this write was SENT during the gap,
   * so it counts here (`postBeats`); the next gap starts from zero.
   */
  function flushSlow(): void {
    flushArmed = false;
    const waiting = pending;
    if (waiting === null) return;
    pending = null;
    const total = waiting.beats + postBeats;
    const worst = Math.max(waiting.beatLate, postLate);
    postBeats = 0;
    postLate = 0;
    beats = 0;
    beatLate = 0;
    emit({
      schema: SCHEMA,
      kind: 'frame',
      run_id: runId,
      ts_ms: Date.now(),
      frame: waiting.frame,
      ms: waiting.gap,
      // The evidence: beats that reached the page across this gap, the
      // WORST lateness among them, when the last one landed, and how many
      // the gap WOULD carry at the tick. **Both axes are needed to read it**:
      // beats equal to expected with LOW lateness is a page that ran while
      // frames stopped (a compositing gate); the same count with the worst
      // lateness approaching the gap is the busy-main-thread burst; one beat
      // against a far larger expected count is the suspended shape; zero
      // against at least one means nothing ticked.
      ...(heartbeatOk
        ? {
            worker_beats: total,
            ...(total > 0 ? { worker_max_late_ms: worst } : {}),
            ...(waiting.sinceBeat === null ? {} : { worker_since_beat_ms: waiting.sinceBeat }),
            worker_expected_beats: Math.floor(waiting.gap / HEARTBEAT_TICK_MS),
            worker_tick_ms: HEARTBEAT_TICK_MS,
          }
        : {}),
      surface: waiting.surface,
      session: waiting.session,
    });
  }

  function tick(): void {
    const at = now();
    frames += 1;
    if (last !== null) {
      const gap = at - last;
      if (gap > maxMs) maxMs = gap;
      if (gap >= slowFrameMs) {
        // Counted where it is MEASURED, so a summary crossing before the
        // deferred record still counts it.
        slow += 1;
        // A slow frame still waiting for its record is written first: a
        // second one is the NEXT period's record, and the beats that reached
        // the page up to now belong to the one being written.
        flushSlow();
        pending = {
          gap,
          frame: frames,
          sinceBeat: lastBeatAt === null ? null : at - lastBeatAt,
          beats,
          beatLate,
          surface,
          session,
        };
        beats = 0;
        beatLate = 0;
        if (!flushArmed) {
          flushArmed = true;
          defer(flushSlow);
        }
      } else {
        // A fast frame that no record is waiting on starts the counters'
        // period over; one a record IS waiting on keeps them (the beats
        // arriving there were sent during that gap).
        if (pending === null) {
          beats = 0;
          beatLate = 0;
        }
        if (openAt !== null && contentSeen) {
          // **The open seat's work is done**: its state is drawn and the
          // frame loop is back to a normal cadence, which is the quiet the
          // switch's own timing is measured to.
          emit({
            schema: SCHEMA,
            kind: 'quiet',
            run_id: runId,
            ts_ms: Date.now(),
            ms: at - openAt,
            surface: 'session',
            session: openSeat,
          });
          openAt = null;
          openSeat = null;
        }
      }
    }
    // A beat before the FIRST frame belongs to no gap, so the baseline
    // frame starts the counters' period as a fast one would.
    if (last === null) {
      beats = 0;
      beatLate = 0;
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
      openSeat = null;
      placeholderSeen = false;
      contentSeen = false;
      windowStart = now();
      // Null until the first tick lands: before then there is no arrival to
      // measure from, and a since-beat of "since start" would invent one.
      lastBeatAt = null;
      pending = null;
      postBeats = 0;
      postLate = 0;
      beats = 0;
      beatLate = 0;
      flushArmed = false;
      failedSeen = false;
      // Boot's own reading: the router has usually not wired a route yet,
      // so null here is the honest answer rather than a gap.
      windowSurface = surface;
      windowSession = session;
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
      cancelHeartbeat = heartbeat(
        (lateMs) => {
          beats += 1;
          if (lateMs > beatLate) beatLate = lateMs;
          if (pending !== null) {
            postBeats += 1;
            if (lateMs > postLate) postLate = lateMs;
          }
          lastBeatAt = now();
        },
        (why) => {
          // **A worker that failed to load is announced, never silent.** Its
          // `error`/`messageerror` means no beat will ever arrive; the fields
          // go absent (not zero) and one record names it, with the event's
          // own error where it has one and its type otherwise, so a broken
          // asset cannot read as a page that stopped.
          if (failedSeen) return;
          failedSeen = true;
          heartbeatOk = false;
          emit({
            schema: SCHEMA,
            kind: 'heartbeat',
            run_id: runId,
            ts_ms: Date.now(),
            heartbeat: false,
            name: why === '' ? 'worker_load_failed' : `worker_load_failed: ${why}`,
            worker_tick_ms: HEARTBEAT_TICK_MS,
            surface,
            session,
          });
        },
      );
      // A construction throw reported synchronously above, so the run line
      // must say "no heartbeat" even though the failure record already landed.
      heartbeatOk = cancelHeartbeat !== null && !failedSeen;
      emit({
        schema: SCHEMA,
        kind: 'run_started',
        run_id: runId,
        ts_ms: Date.now(),
        version,
        heartbeat: heartbeatOk,
        ...(heartbeatOk ? { worker_tick_ms: HEARTBEAT_TICK_MS } : {}),
        surface,
        session,
      });
      cancelFrame = schedule(tick);
    },
    stop(): void {
      // The waiting slow frame's record still lands: a partial window is
      // what the reader wants when the page is going away.
      flushSlow();
      flushWindow(now());
      cancelFrame?.();
      cancelObserver?.();
      cancelHeartbeat?.();
      cancelFrame = null;
      cancelObserver = null;
      cancelHeartbeat = null;
    },
    setSurface(next: string | null, seat: string | null): void {
      surface = next;
      session = seat;
      // **An open belongs to the seat that opened it.** Leaving that seat -
      // to the home, a board, a view that opens no conversation, or another
      // seat - ends an unfinished open rather than letting its quiet land
      // under whatever came next.
      if (openAt !== null && seat !== openSeat) {
        openAt = null;
        openSeat = null;
      }
    },
    noteConversationOpened(seat: string): void {
      openSeat = seat;
      openAt = now();
      placeholderSeen = false;
      contentSeen = false;
      emit({
        schema: SCHEMA,
        kind: 'switch',
        run_id: runId,
        ts_ms: Date.now(),
        surface: 'session',
        session: seat,
      });
    },
    noteDrawn(state: DrawnState): void {
      if (openAt === null) return;
      const content = state === 'conversation' || state === 'empty';
      // **One placeholder and one content drawing per open** (#2374): the
      // column re-runs the mark on every update it holds, so the first of
      // each is what the log carries.
      if (content ? contentSeen : placeholderSeen) return;
      if (content) {
        contentSeen = true;
      } else {
        placeholderSeen = true;
      }
      emit({
        schema: SCHEMA,
        kind: 'drawn',
        run_id: runId,
        ts_ms: Date.now(),
        ms: now() - openAt,
        state,
        surface: 'session',
        session: openSeat,
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
 * The seat as the perf log names it. **The ONE place the string is built**:
 * a frame's surface pair and the marks a conversation's open carries must
 * name the seat identically, or a reader pairing them by it finds nothing
 * (#2374).
 */
export function seatLabel(slot: SessionSlot): string {
  return `${slot.org}/${slot.project}/${slot.label}`;
}

/**
 * A conversation has opened for the seat `slot`. **The open is written where
 * the column takes the seat on**, not where a route changed: a click, a deep
 * link, a reload, a reconnect and a palette move all pass through the
 * column, and a route change alone is no conversation - a board is not a
 * seat and a board route must write nothing (#2374).
 */
export function noteConversationOpened(slot: SessionSlot): void {
  running?.noteConversationOpened(seatLabel(slot));
}

/**
 * The open seat's terminal drawing has landed, in `state`: its conversation,
 * the empty state, or the column's own waking or refused line. The mark a
 * switch's own timing is measured to, written where the drawing lands rather
 * than where the click happened (#2374). **An open can carry two of these**:
 * a placeholder first (`waking`/`refused`) and the seat's content after it
 * (`conversation`/`empty`) - only the content drawing lets a `quiet` follow.
 */
export function noteDrawn(state: DrawnState): void {
  running?.noteDrawn(state);
}

/**
 * A route, as the probe's records name it: the surface is the route's own
 * name, and the session is the seat, when the route holds one. #1596 asks
 * that a record say which surface and which session a cost came from - this
 * is the pair that does it.
 */
export function perfSurface(route: Route): { surface: string; session: string | null } {
  if ('slot' in route) {
    return { surface: route.name, session: seatLabel(route.slot) };
  }
  if (route.name === 'board') return { surface: 'board', session: `${route.org}/${route.project}` };
  return { surface: route.name, session: null };
}

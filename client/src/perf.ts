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
 * **Where a record is written is the platform's one difference**: the shell's
 * invoke writes it beside the shell's own log, and a page with no shell sends
 * the same batch over its socket. Nothing here may break the app: a refused
 * write or a batch with no live connection is silence, never an error thrown
 * into a render path.
 */

import { invoke, isTauri } from '@tauri-apps/api/core';

import { HEARTBEAT_TICK_MS } from './heartbeat';
import { cannotPaint } from './paint';
import { CLIENT_VERSION } from './protocol';
import type { Subject } from './protocol';
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

/**
 * How recent the frame before a clock tick must be for the tick's span to be
 * the write's cost.
 *
 * **A longer gap means the loop was dead**, not slow: the span would be the
 * dead period rather than the write's cost. 250 ms tolerates a page as slow
 * as 4 fps.
 */
const DEAD_FRAME_MS = 250;

/** How often a window's cadence is summarised, the same cadence the view this
 *  client replaced used. */
const SUMMARY_INTERVAL_MS = 10_000;

/**
 * How often the connection's own counters are written while something is
 * happening: a subscribe burst is read at this cadence, not the summary's ten
 * seconds (#2513).
 */
const CONNECTION_WINDOW_MS = 1_000;

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

/**
 * What the column measured where a drawn mark is written.
 *
 * **A drawn mark is a state, not a pixel**: it is read off the record, so a
 * frame whose rows reached the model and not the screen still marks as drawn.
 * The geometry is what tells the two apart - the viewport's own scroll
 * numbers, the turns in the DOM against those with a box, and whether the
 * foot's pin refused itself on an undrawn newest row.
 */
export interface DrawnGeometry {
  scroll_top: number;
  scroll_height: number;
  client_height: number;
  /** Turns the list rendered, and how many of them the browser gave a box. */
  rows: number;
  rows_visible: number;
  pin_refused: boolean;
}

/** One line of the perf log, as the shell writes it. */
export interface PerfRecord {
  schema: string;
  kind:
    | 'run_started'
    | 'frame'
    | 'longtask'
    | 'summary'
    | 'switch'
    | 'drawn'
    | 'painted'
    | 'quiet'
    | 'heartbeat'
    | 'click'
    | 'subscribe'
    | 'snapshot'
    | 'connection'
    | 'input'
    | 'paint'
    | 'tick';
  run_id: string;
  ts_ms: number;
  version?: string;
  /** The frame's own counter within the run, for a frame record. */
  frame?: number;
  /**
   * The frame's gap, or the long task's duration, or - for a `drawn`, a
   * `painted` and a `quiet` - the time since the switch record the mark
   * belongs to. A `tick` reads the span from the page clock's write to the
   * next frame callback: an upper bound, because the wait for that frame is
   * on it, and so is anything else that wrote in the window.
   */
  ms?: number;
  /**
   * For a `drawn`: WHICH terminal state completed the open. Only
   * `conversation` and `empty` are the seat's own answer; a `waking` or
   * `refused` line is the column saying something else, and must never read
   * as the content arriving (#2374).
   */
  state?: DrawnState;
  /** A `drawn`: what the column measured when the mark was written, so a
   *  frame that marked drawn and painted nothing names itself. */
  scroll_top?: number;
  scroll_height?: number;
  client_height?: number;
  rows?: number;
  rows_visible?: number;
  pin_refused?: boolean;
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
  /**
   * A `subscribe`: whether it followed a snapshot's completion with no user
   * action before it - the repeated re-subscribe shape a switch's delay can
   * hide in (#2513). A click or a conversation opening counts as the action.
   */
  auto?: boolean;
  /** A `subscribe`: the declare flag the frame carried. */
  declare?: boolean;
  /** A `subscribe` on the desktop: how long the shell half held it (its
   *  queue plus the socket write) before it went (#2513). Absent on the web
   *  build, where the page writes the frame itself. */
  write_waited_ms?: number;
  /** A `subscribe`: ms since the click that preceded it; absent when no
   *  click did, so a reconnect never invents one. */
  since_click_ms?: number;
  /** A `frame` or a `tick`: whether the span it measures overlapped a hidden
   *  window, measured when it is timed - a visibility gate rather than a
   *  blocked thread (#2569). */
  hidden?: boolean;
  /**
   * A `snapshot`'s JSON length in string units (UTF-16 code units), which is
   * what the boundary parse walked without a second pass over it (#2513). A
   * frame that crossed compressed reports its inflated length here; the
   * bytes it cost on the wire are the server's `frame_write` line when the
   * frame was a seat's open - a home or board read has no such line.
   */
  chars?: number;
  /** A `snapshot`: how long its JSON.parse took, at the boundary. */
  parse_ms?: number;
  /** A `connection` window's counts: subscribes sent, snapshot chars read. */
  subscribes?: number;
  snapshot_chars?: number;
  /** The same counts per second of the window, which is the rate a burst is
   *  read at (#2513). */
  subscribe_rate?: number;
  snapshot_chars_per_s?: number;
  /**
   * A browser mark's page: the profile its tab belongs to and the tab's own
   * number. **The pair is what a gesture and the frame that answers it are
   * paired by**, since a tab number alone is one per profile.
   */
  profile?: string | null;
  tab?: number;
  /** A `paint`: the frame's own sequence on its tab. */
  seq?: number;
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
  /**
   * The open seat's terminal drawing has landed, in `state` (#2374), with
   * `geometry` where the mark is the rows' own.
   *
   * **`geometry` is a thunk because measuring the column forces layout**, and
   * the column marks a state on every update it holds while the probe writes
   * one record per open: called here, the measure runs only on the write.
   */
  noteDrawn(state: DrawnState, geometry?: () => DrawnGeometry | undefined): void;
  /**
   * The open seat's first row got a box on screen.
   *
   * **The drawn mark is a state, not a pixel**: it is written on the flush that
   * first holds the seat's turns, before that flush's own DOM exists, so it
   * cannot say when a reader first had a row to look at. This is the other
   * half, and it is what tells an open that drew at 80 ms and painted at 10 s
   * from one that was actually on screen (#2733, #2752, #1909).
   */
  notePainted(): void;
  /** A click on a seat, at the handler that takes it (#2513). */
  noteSeatClick(seat: string): void;
  /** A subscribe written to the socket, with the seat its subject names when
   *  it names one (#2513). `write_waited_ms` is the desktop shell's own
   *  queue-and-write time, absent where the page wrote the frame itself. */
  noteSubscribe(seat: string | null, declare: boolean, writeWaitedMs?: number): void;
  /** A snapshot frame, after its JSON.parse: the frame's length and how long
   *  the parse took (#2513). */
  noteSnapshot(seat: string | null, chars: number, parseMs: number): void;
  /** One gesture handed to the browser, as it leaves this side (#2645). */
  noteBrowserInput(kind: string, tab: number, profile: string | null): void;
  /**
   * The page clock's state write, timed to the next frame; see the `tick`
   * reading on `ms`. Written only when the span is slow and the frame before
   * the write is alive.
   */
  noteClockTick(seat: string | null): void;
  /** One browser frame drawn for `tab`, with its own sequence (#2645). */
  noteBrowserPaint(seq: number, tab: number, profile: string | null): void;
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
  /** Reports the page's visibility as it changes; returns its own disconnect,
   *  or null where there is no document - which keeps the flag a fact rather
   *  than a claim (#2569). */
  observeVisibility?: (cb: (hidden: boolean) => void) => (() => void) | null;
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

/**
 * Reports the page's visibility as it changes; returns its own disconnect,
 * or null where there is no document.
 *
 * **The event, not a poll**: a hidden window produces no frames at all, so
 * the only way to know a gap overlapped one is to hear it end. Reading
 * `document.hidden` at the deferred write would read the state AFTER the
 * return - the opposite of the diagnosis (#2569).
 */
function observeVisibility(cb: (hidden: boolean) => void): (() => void) | null {
  if (typeof document === 'undefined') return null;
  const report = (): void => cb(cannotPaint());
  document.addEventListener('visibilitychange', report);
  return () => document.removeEventListener('visibilitychange', report);
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

/**
 * Records waiting for this turn's one invoke, and whether one is queued.
 *
 * **One write per turn, not one per record.** The shell command behind this
 * opens, appends and closes a file, and a burst of marks (the home loop's
 * subscribe and snapshot, a window's summary beside a slow frame) would
 * otherwise put that IO on the main thread once per mark - on the path this
 * probe measures (#2513).
 */
let pendingWrites: PerfRecord[] = [];
let writeQueued = false;

/** The page's own sink: the connection it posts through, once there is one. */
let socketSink: ((records: PerfRecord[]) => boolean) | null = null;

/** The run's opening line, held until a connection can carry it. */
let startLine: PerfRecord | null = null;

/**
 * Point the web build's sink at the connection whose socket carries its
 * records. `null` - no connection - drops them.
 *
 * **The run's own line goes with the first connection.** The probe starts at
 * module load, long before a socket exists, so its `run_started` would
 * otherwise be dropped and a web log would open mid-story.
 */
export function setPerfSocket(sink: ((records: PerfRecord[]) => boolean) | null): void {
  socketSink = sink;
  if (sink !== null && startLine !== null) {
    const line = startLine;
    startLine = null;
    sink([line]);
  }
}

/**
 * The web build's write: one batch over the socket per turn.
 *
 * **A batch with no live connection is dropped, not queued**: the log is a
 * diagnostic, and a page that hoards records through an outage is measuring
 * its own backlog rather than the frames.
 */
function sinkToSocket(records: PerfRecord[]): void {
  pendingWrites.push(...records);
  // **The opening line is not a loss**: it describes the run, so it is held
  // until a connection exists rather than going out with a batch nobody
  // carries.
  if (startLine === null) {
    startLine = pendingWrites.find((record) => record.kind === 'run_started') ?? null;
  }
  if (writeQueued) return;
  writeQueued = true;
  queueMicrotask(() => {
    writeQueued = false;
    const batch = pendingWrites;
    pendingWrites = [];
    socketSink?.(batch);
  });
}

/** The sink this build writes through. */
export function perfSink(): (records: PerfRecord[]) => void {
  return isTauri() ? sinkToShell : sinkToSocket;
}

/** The shell's write: one invoke per turn, fire-and-forget. */
function sinkToShell(records: PerfRecord[]): void {
  pendingWrites.push(...records);
  if (writeQueued) return;
  writeQueued = true;
  queueMicrotask(() => {
    writeQueued = false;
    const batch = pendingWrites;
    pendingWrites = [];
    void invoke('client_perf', { records: batch }).catch(() => {
      // A refused write is a lost record, not a broken page: the shell may be
      // gone, and a perf sink that threw would take a render path with it.
    });
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
    /** Whether this gap overlapped a hidden period, sampled at measurement. */
    hidden: boolean;
    surface: string | null;
    session: string | null;
  } | null = null;
  let cancelVisibility: (() => void) | null = null;
  /**
   * Whether the page is hidden now, and when it last stopped being hidden.
   *
   * **A hidden window produces no frames**, so the gap it causes is measured
   * on the first frame after the page returns, when visibility has already
   * flipped back. The end of the hidden period is what says the gap
   * overlapped it; the deferred write cannot (#2569).
   */
  let hiddenNow = typeof document !== 'undefined' && document.hidden;
  let hiddenEndedAt: number | null = null;
  const noteVisibility = (hidden: boolean): void => {
    if (hidden === hiddenNow) return;
    hiddenNow = hidden;
    if (!hidden) hiddenEndedAt = now();
  };
  /** Whether the gap that opened at `from` overlapped a hidden period. */
  function gapHidden(from: number): boolean {
    return hiddenNow || (hiddenEndedAt !== null && hiddenEndedAt >= from);
  }
  let postBeats = 0;
  let postLate = 0;
  let flushArmed = false;
  /** The page clock's last state write, waiting for the frame it costs. */
  let clockTick: { at: number; seat: string | null; surface: string | null } | null = null;
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
  /**
   * The open whose first row has not painted yet, when it opened, and whether
   * its mark has been written.
   *
   * **A clock of its own, not `openAt`'s.** A quiet can settle an open before
   * its rows paint - which is exactly the case the painted mark exists for
   * (#2733, #2752) - so the span survives the quiet and is dropped only when
   * the reader leaves the seat or a new open starts.
   */
  let paintSeat: string | null = null;
  let paintAt: number | null = null;
  let paintSeen = false;
  /**
   * #2513's own counters: the click a subscribe is measured from, the last
   * user action and the last snapshot's completion (what makes a subscribe
   * "unprompted"), and the connection window's subscribes and snapshot chars.
   */
  /**
   * The last seat click, and the seat it named.
   *
   * **A click is spent only by a subscribe of its own seat.** A seat the
   * client already holds opens no subscribe - the one-subscribe rule
   * guarantees it - so a bare timestamp outlived the click and the next
   * automatic subscribe of any subject reported the old click's age as its
   * own gap (measured: 18,006 ms on a session-less home read).
   */
  let clickAt: { at: number; seat: string } | null = null;
  let lastUserActionAt: number | null = null;
  let lastSnapshotAt: number | null = null;
  let connStart = 0;
  let connSubscribes = 0;
  let connSnapshotChars = 0;
  /**
   * The page the window's newest count happened on.
   *
   * **Stamped where the count is taken.** These used to be set when a window
   * OPENED, so a record named the seat the reader had already left and not the
   * one whose subscribes and snapshots it carried: a 3.17 MB answer read as
   * one seat's while it was another's record and a home read.
   */
  let connSurface: string | null = null;
  let connSession: string | null = null;
  /**
   * The tab a gesture is waiting on a frame from: **the round trip is the
   * first frame drawn after the input**, so the later ones are not written
   * (#2645). One line per frame would drown a file nobody reads until
   * something is wrong.
   */
  let awaitingPaint: number | null = null;

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
      // **What tells a gap on an occluded window from a stuck thread**, the
      // same fact `paint.ts` reads, sampled when the gap was measured: a
      // hidden page is producing no frames at all, so its gap is the
      // visibility gate rather than blocked work.
      hidden: waiting.hidden,
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

  /**
   * Write the connection window's counts, if anything happened in it. A quiet
   * second is the summary's to say, not a record of its own.
   */
  function flushConnection(at: number): void {
    const windowMs = at - connStart;
    if (connSubscribes === 0 && connSnapshotChars === 0) {
      connStart = at;
      return;
    }
    const seconds = windowMs / 1000;
    emit({
      schema: SCHEMA,
      kind: 'connection',
      run_id: runId,
      ts_ms: Date.now(),
      window_ms: windowMs,
      subscribes: connSubscribes,
      snapshot_chars: connSnapshotChars,
      subscribe_rate: seconds > 0 ? connSubscribes / seconds : 0,
      snapshot_chars_per_s: seconds > 0 ? connSnapshotChars / seconds : 0,
      surface: connSurface,
      session: connSession,
    });
    connSubscribes = 0;
    connSnapshotChars = 0;
    connStart = at;
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
          hidden: gapHidden(last),
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
    // **A tick that lands while frames were not running cannot claim the
    // dead period.** The frame before the write must be recent, and a span
    // that overlapped a hidden window is named the way a slow frame's is.
    if (clockTick !== null) {
      const cost = at - clockTick.at;
      const live = last !== null && clockTick.at - last <= DEAD_FRAME_MS;
      if (live && cost >= slowFrameMs) {
        emit({
          schema: SCHEMA,
          kind: 'tick',
          run_id: runId,
          ts_ms: Date.now(),
          ms: cost,
          hidden: gapHidden(clockTick.at),
          surface: clockTick.surface,
          session: clockTick.seat,
        });
      }
      clockTick = null;
    }
    // A beat before the FIRST frame belongs to no gap, so the baseline
    // frame starts the counters' period as a fast one would.
    if (last === null) {
      beats = 0;
      beatLate = 0;
    }
    last = at;
    if (at - connStart >= CONNECTION_WINDOW_MS) flushConnection(at);
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
      paintSeat = null;
      paintAt = null;
      paintSeen = false;
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
      clickAt = null;
      lastUserActionAt = null;
      lastSnapshotAt = null;
      connStart = now();
      connSubscribes = 0;
      connSnapshotChars = 0;
      awaitingPaint = null;
      clockTick = null;
      // Boot's own reading: the router has usually not wired a route yet,
      // so null here is the honest answer rather than a gap.
      windowSurface = surface;
      windowSession = session;
      connSurface = surface;
      connSession = session;
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
      hiddenNow = typeof document !== 'undefined' && document.hidden;
      hiddenEndedAt = null;
      cancelVisibility = (options.observeVisibility ?? observeVisibility)?.(noteVisibility) ?? null;
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
      flushConnection(now());
      cancelFrame?.();
      cancelObserver?.();
      cancelHeartbeat?.();
      cancelVisibility?.();
      cancelFrame = null;
      cancelObserver = null;
      cancelHeartbeat = null;
      cancelVisibility = null;
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
      // The painted mark belongs to the seat that opened, so leaving it drops
      // the span rather than stamping the next seat's rows with this one's.
      if (paintSeat !== null && seat !== paintSeat) {
        paintSeat = null;
        paintAt = null;
      }
    },
    noteConversationOpened(seat: string): void {
      openSeat = seat;
      openAt = now();
      paintSeat = seat;
      paintAt = openAt;
      paintSeen = false;
      // A click for the seat the reader left opens nothing here, so it is
      // spent rather than left to a later read (#2708).
      if (clickAt !== null && clickAt.seat !== seat) clickAt = null;
      // A conversation opening is a user action (a deep link, a reload, a
      // palette move), so the subscribe it leads to is not "unprompted".
      lastUserActionAt = openAt;
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
    noteDrawn(state: DrawnState, geometry?: () => DrawnGeometry | undefined): void {
      if (openAt === null) return;
      const content = state === 'conversation' || state === 'empty';
      // **One placeholder and one content drawing per open** (#2374): the
      // column re-runs the mark on every update it holds, so the first of
      // each is what the log carries.
      if (content ? contentSeen : placeholderSeen) return;
      if (content) {
        contentSeen = true;
        // **An empty seat has no row to paint** (#2754 review): the open is
        // answered by the empty state itself, and its first message can
        // measure minutes later - holding the arm would stamp that whole wait
        // as the open's paint. A placeholder keeps it, because the content
        // drawing that follows brings the rows.
        if (state === 'empty') {
          paintSeat = null;
          paintAt = null;
        }
        // **The seat's content drew without a subscribe spending the click**:
        // a seat the client already holds is drawn from what it kept and
        // subscribes nothing, so its open is what says the click opened no
        // read. A later subscribe would otherwise report this click's age as
        // its own gap (#2708). The placeholder is not this mark: a cold
        // seat's waking line draws before the subscribe still to come.
        if (clickAt !== null && clickAt.seat === openSeat) clickAt = null;
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
        ...(geometry?.() ?? {}),
        surface: 'session',
        session: openSeat,
      });
    },
    notePainted(): void {
      if (paintSeat === null || paintAt === null || paintSeen) return;
      paintSeen = true;
      emit({
        schema: SCHEMA,
        kind: 'painted',
        run_id: runId,
        ts_ms: Date.now(),
        ms: now() - paintAt,
        surface: 'session',
        session: paintSeat,
      });
    },
    noteSeatClick(seat: string): void {
      const at = now();
      // **A click on the seat already on screen opens nothing**: no switch,
      // no read, so arming it would hand a later reconnect an old gap as its
      // own. The click record is still written - it is a real action.
      clickAt = seat === session ? null : { at, seat };
      lastUserActionAt = at;
      emit({
        schema: SCHEMA,
        kind: 'click',
        run_id: runId,
        ts_ms: Date.now(),
        surface: 'session',
        session: seat,
      });
    },
    noteSubscribe(seat: string | null, declare: boolean, writeWaitedMs?: number): void {
      const at = now();
      // **An unprompted subscribe follows a snapshot's completion.** No click
      // or conversation opening happened after that completion, which is the
      // repeated re-subscribe shape a switch's delay can hide in (#2513).
      const auto = lastSnapshotAt !== null && lastSnapshotAt > (lastUserActionAt ?? -Infinity);
      connSubscribes += 1;
      connSurface = surface;
      connSession = session;
      // **A click is spent by the first subscribe of its OWN seat.** A
      // subscribe for another seat supersedes it, because the reader went
      // elsewhere; a sessionless subscribe - the automatic home loop - is no
      // seat switch and leaves the click armed, so an unrelated ack landing
      // first cannot cost the click the seat switch it was measuring
      // (#2513, #2708).
      const clicked = clickAt;
      const own = clicked !== null && seat !== null && clicked.seat === seat;
      if (own || seat !== null) clickAt = null;
      const sinceClick = own && clicked !== null ? at - clicked.at : undefined;
      emit({
        schema: SCHEMA,
        kind: 'subscribe',
        run_id: runId,
        ts_ms: Date.now(),
        declare,
        auto,
        ...(writeWaitedMs === undefined ? {} : { write_waited_ms: writeWaitedMs }),
        ...(sinceClick === undefined ? {} : { since_click_ms: sinceClick }),
        ...(seat === null ? {} : { session: seat }),
        surface,
      });
    },
    noteSnapshot(seat: string | null, chars: number, parseMs: number): void {
      lastSnapshotAt = now();
      connSnapshotChars += chars;
      connSurface = surface;
      connSession = session;
      emit({
        schema: SCHEMA,
        kind: 'snapshot',
        run_id: runId,
        ts_ms: Date.now(),
        chars,
        parse_ms: parseMs,
        ...(seat === null ? {} : { session: seat }),
        surface,
      });
    },
    noteBrowserInput(kind: string, tab: number, profile: string | null): void {
      awaitingPaint = tab;
      emit({
        schema: SCHEMA,
        kind: 'input',
        run_id: runId,
        ts_ms: Date.now(),
        name: kind,
        tab,
        profile,
        ...(session === null ? {} : { session }),
        surface: 'browser',
      });
    },
    noteBrowserPaint(seq: number, tab: number, profile: string | null): void {
      if (awaitingPaint === null || awaitingPaint !== tab) return;
      awaitingPaint = null;
      emit({
        schema: SCHEMA,
        kind: 'paint',
        run_id: runId,
        ts_ms: Date.now(),
        seq,
        tab,
        profile,
        surface: 'browser',
      });
    },
    noteClockTick(seat: string | null): void {
      clockTick = { at: now(), seat, surface };
    },
  };
}

/** The running probe, once one was started. */
let running: PerfProbe | null = null;

/**
 * Start the probe, once. **Every client measures**: the shell writes its
 * records through its own invoke, and a page with no shell sends the same
 * batches over the socket, where the server writes them beside its own log.
 */
export function startPerfProbe(options: ProbeOptions = {}): PerfProbe | null {
  if (running !== null) return running;
  running = perfProbe({ ...options, sink: options.sink ?? perfSink() });
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
 *
 * `geometry` rides the rows' own mark, which is where a drawn state and a
 * drawn pixel can disagree.
 */
export function noteDrawn(state: DrawnState, geometry?: () => DrawnGeometry | undefined): void {
  running?.noteDrawn(state, geometry);
}

/**
 * The open seat's first row got a box on screen. **The other half of the drawn
 * mark**: that one is the model, this one is the pixels, and an open that drew
 * fast and painted late is the shape #2733 and #2752 were filed on.
 */
export function notePainted(): void {
  running?.notePainted();
}

/** A click landed on `slot`, in the shell's delegated handler (#2513). */
export function noteSeatClick(slot: SessionSlot): void {
  running?.noteSeatClick(seatLabel(slot));
}

/**
 * One subscribe was written to the socket (#2513). A string subject names no
 * seat, so its record carries none.
 */
export function noteSubscribe(subject: Subject, declare: boolean): void {
  running?.noteSubscribe(typeof subject === 'string' ? null : seatLabel(subject.session), declare);
}

/**
 * One subscribe the DESKTOP's shell half wrote (#2513): the seat comes from
 * the shell's own subject key, and `waitedMs` is how long that half held the
 * frame between the page's command and the socket.
 */
export function noteSubscribeWritten(
  seat: string | null,
  declare: boolean,
  waitedMs: number,
): void {
  running?.noteSubscribe(seat, declare, waitedMs);
}

/** One snapshot frame was parsed, with its length and parse time (#2513). */
export function noteSnapshot(subject: Subject, chars: number, parseMs: number): void {
  running?.noteSnapshot(
    typeof subject === 'string' ? null : seatLabel(subject.session),
    chars,
    parseMs,
  );
}

/**
 * The page clock's state write, timed to the next frame (#2782); see `ms`.
 */
export function noteClockTick(slot: SessionSlot): void {
  running?.noteClockTick(seatLabel(slot));
}

/** One gesture on its way to the browser, as the page's or the shell's path sends it (#2645). */
export function noteBrowserInput(kind: string, tab: number, profile: string | null): void {
  running?.noteBrowserInput(kind, tab, profile);
}

/** One browser frame drawn for `tab`; only the first after a gesture is written (#2645). */
export function noteBrowserPaint(seq: number, tab: number, profile: string | null): void {
  running?.noteBrowserPaint(seq, tab, profile);
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

import { describe, expect, it, vi } from 'vitest';

import { perfProbe, perfSurface, seatLabel, startPerfProbe, type PerfRecord } from './perf';

/**
 * The probe's clock and frame source, driven by the test.
 *
 * The core is injected with these rather than reaching for `performance` and
 * `requestAnimationFrame` itself, which is the whole reason a known slow
 * frame can be produced here instead of hoped for on a machine.
 */
function harness(options: { summaryIntervalMs?: number } = {}) {
  const records: PerfRecord[][] = [];
  let clock = 1_000;
  const callbacks: (() => void)[] = [];
  let longTask: ((ms: number, name: string) => void) | null = null;
  let heartbeat: ((lateMs: number) => void) | null = null;
  let heartbeatFailed: ((why: string) => void) | null = null;
  const deferred: (() => void)[] = [];
  let stopped = false;

  const probe = perfProbe({
    now: () => clock,
    schedule: (cb) => {
      callbacks.push(cb);
      return () => {
        stopped = true;
      };
    },
    observeLongTasks: (cb) => {
      longTask = cb;
      return () => {
        longTask = null;
      };
    },
    observeHeartbeat: (cb, failed) => {
      heartbeat = cb;
      heartbeatFailed = failed;
      return () => {
        heartbeat = null;
        heartbeatFailed = null;
      };
    },
    // The record for a slow frame is written one macrotask after it, so a
    // test owns that turn: `settle()` runs every deferred callback.
    defer: (cb) => {
      deferred.push(cb);
    },
    sink: (batch) => {
      records.push(batch);
    },
    ...(options.summaryIntervalMs === undefined
      ? {}
      : { summaryIntervalMs: options.summaryIntervalMs }),
  });
  // The order main.ts has: the probe starts at boot, and the router wires
  // the route a moment later - so the run line's reading is boot's own.
  probe.start();

  /** One frame's arrival, `ms` after the previous one. */
  const frame = (ms: number) => {
    clock += ms;
    const cb = callbacks.shift();
    if (cb === undefined) throw new Error('no frame is scheduled');
    cb();
  };

  return {
    probe,
    frame,
    records,
    longTask: (ms: number, name = 'self') => longTask?.(ms, name),
    beat: (lateMs = 5) => heartbeat?.(lateMs),
    failHeartbeat: (why = '') => heartbeatFailed?.(why),
    /** The macrotask turn the slow frame's record is written on. */
    settle: () => {
      for (let cb = deferred.shift(); cb !== undefined; cb = deferred.shift()) cb();
    },
    get beating() {
      return heartbeat !== null;
    },
    get stopped() {
      return stopped;
    },
  };
}

describe('the frame probe', () => {
  it('opens its log with a run line carrying the boot reading', () => {
    const h = harness();
    const first = h.records.flat()[0];
    expect(first?.kind).toBe('run_started');
    expect(first?.schema).toBe('forge-perf/v1');
    expect(first?.run_id).toBeTruthy();
    expect(first?.version).toBeTruthy();
    // The router wires the route after this line, so null is boot's own
    // reading rather than a missing field.
    expect(first?.surface).toBeNull();
    expect(first?.session).toBeNull();

    // And once the route IS wired, the frames carry it.
    h.probe.setSurface('connect', null);
    h.frame(16);
    h.frame(120);
    h.settle(); // the slow frame's record lands on the next macrotask
    const frame = h.records.flat().find((record) => record.kind === 'frame');
    expect(frame?.surface).toBe('connect');
  });

  it('records a slow frame and nothing for the fast ones between', () => {
    const h = harness();
    h.probe.setSurface('session', 'forge/lead');
    h.frame(16);
    h.frame(16);
    h.frame(120);
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames).toHaveLength(1);
    expect(frames[0]?.ms).toBe(120);
    expect(frames[0]?.surface).toBe('session');
    expect(frames[0]?.session).toBe('forge/lead');
    expect(frames[0]?.frame).toBe(3);
  });

  it('counts the threshold itself as slow, the way the terminal does', () => {
    const h = harness();
    h.frame(49.9);
    expect(h.records.flat().filter((record) => record.kind === 'frame')).toHaveLength(0);
    h.frame(50);
    h.settle();
    expect(h.records.flat().filter((record) => record.kind === 'frame')).toHaveLength(1);
  });

  it('summarises a window even when nothing in it was slow', () => {
    const h = harness({ summaryIntervalMs: 100 });
    h.frame(16);
    h.frame(16);
    h.frame(200);
    h.frame(16);

    const summaries = h.records.flat().filter((record) => record.kind === 'summary');
    // The window covers the frames up to the one that crossed the interval,
    // the same count-then-check rule the view this client replaced applied.
    expect(summaries).toHaveLength(1);
    expect(summaries[0]?.frames).toBe(3);
    expect(summaries[0]?.slow).toBe(1);
    expect(summaries[0]?.max_ms).toBe(200);
  });

  it('names the surface its window OPENED under, even when the seat changes inside it', () => {
    const h = harness({ summaryIntervalMs: 100 });
    h.probe.setSurface('home', null);
    h.frame(60);
    h.frame(60); // window 1 is summarised here; window 2 opens under `home`.

    // The switch lands INSIDE window 2, before its crossing.
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.frame(120); // window 2's crossing: its summary must still say `home`.
    h.settle();

    const summaries = h.records.flat().filter((record) => record.kind === 'summary');
    expect(summaries).toHaveLength(2);
    expect(summaries[0]?.surface, 'window 1 opened at boot, before the route was wired').toBeNull();
    expect(
      summaries[1]?.surface,
      'the summary says where its window opened, not where it closed',
    ).toBe('home');
    expect(summaries[1]?.session).toBeNull();

    // The frame after the switch carries where IT happened.
    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    const after = frames[frames.length - 1];
    expect(after?.ms).toBe(120);
    expect(after?.surface).toBe('session');
    expect(after?.session).toBe('Busytools/forge/lead');
  });

  it('lands the open window when stopped, the terminal rule at shutdown', () => {
    const h = harness();
    h.frame(16);
    h.frame(16);
    h.probe.stop();

    const summaries = h.records.flat().filter((record) => record.kind === 'summary');
    expect(summaries).toHaveLength(1);
    expect(summaries[0]?.frames, 'a partial window is not dropped').toBe(2);
  });

  it('starts nothing where there is no shell to write to', () => {
    // The vitest page has no `isTauri`, which is the web build's own state:
    // a record with nothing behind it would help no one.
    expect(startPerfProbe()).toBeNull();
  });

  it('carries a long task with its own name', () => {
    const h = harness();
    h.probe.setSurface('home', null);
    h.longTask(90, 'self');

    const tasks = h.records.flat().filter((record) => record.kind === 'longtask');
    expect(tasks).toHaveLength(1);
    expect(tasks[0]?.ms).toBe(90);
    expect(tasks[0]?.name).toBe('self');
    expect(tasks[0]?.surface).toBe('home');
  });

  it('stops measuring where it was told to', () => {
    const h = harness();
    h.probe.stop();
    expect(h.stopped).toBe(true);
  });

  /**
   * **A slow frame's record waits one macrotask, and takes the burst with
   * it** (#2374). A blocked gap's queued ticks are delivered AFTER the frame
   * callback that measured the gap - the defect this test pins: feeding the
   * burst between the frame and the flush, as the engine does, must still
   * land every tick of it on the record, with the WORST lateness (the burst
   * is fed oldest-first so a last-wins reading fails).
   */
  it('writes the slow record after the burst lands, with its beats and worst lateness', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');

    h.beat(3);
    h.beat(7);
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'frame')).toHaveLength(0);

    // The gap is measured first…
    h.frame(5000);
    // …and the blocked thread's queued ticks are delivered right after it.
    h.beat(2400);
    h.beat(1200);
    h.beat(2);
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames).toHaveLength(1);
    expect(frames[0]?.worker_beats, 'the burst is on the record too').toBe(3);
    expect(frames[0]?.worker_max_late_ms, 'the WORST lateness, not the last one').toBe(2400);
    expect(frames[0]?.worker_expected_beats, 'five ticks were due over five seconds').toBe(5);
    expect(frames[0]?.worker_tick_ms, 'and the tick it is read against').toBe(1000);
  });

  /**
   * **The ticks that arrived BEFORE the slow frame keep their worst
   * lateness.** A last-wins reading survives the post-frame burst alone; a
   * descending pre-frame burst (2,400 then 2) fails it.
   */
  it('keeps the worst lateness of a pre-frame burst', () => {
    const h = harness();
    h.frame(16);
    h.beat(2400);
    h.beat(2);
    h.frame(5000);
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames[0]?.worker_beats).toBe(2);
    expect(frames[0]?.worker_max_late_ms, 'the worst, not the last').toBe(2400);
  });

  /** **A second slow frame before the deferred write does not drop it.** */
  it('writes both records when a second slow frame arrives before the flush', () => {
    const h = harness();
    h.frame(16);
    h.frame(200); // slow 1: its record waits a macrotask…
    h.frame(200); // …and slow 2 arrives first
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames, 'neither slow frame is lost').toHaveLength(2);
  });

  /** **A fast frame with no record waiting starts the counters over.** */
  it('does not carry a fast frame with a beat into the next slow gap', () => {
    const h = harness();
    h.frame(16);
    h.beat(5); // lands during a fast frame…
    h.frame(16);
    h.frame(5000); // …which must not be on this gap's record
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames[0]?.worker_beats, 'the normal-cadence beat belongs to no gap').toBe(0);
  });

  /** **A record still waiting when the probe stops is written, not lost.** */
  it('writes a waiting slow record on stop', () => {
    const h = harness();
    h.frame(16);
    h.frame(5000);
    h.probe.stop();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames, 'the deferred record lands at shutdown').toHaveLength(1);
  });

  /** The expected count is the FLOOR of the gap's tick periods. */
  it('counts expected beats by whole tick periods', () => {
    const h = harness();
    h.frame(16);
    h.frame(60); // a slow gap shorter than a tick: no tick was due
    h.settle();
    h.frame(2999);
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames).toHaveLength(2);
    expect(frames[0]?.worker_expected_beats, 'a sub-tick gap expects none').toBe(0);
    expect(frames[1]?.worker_expected_beats, '2,999 ms is two whole periods').toBe(2);
  });

  /** **The seat is captured with the gap**: a route move before the deferred
   *  write does not re-attribute the record. */
  it('names the seat the gap happened on, even after the page moved', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.frame(16);
    h.frame(5000);
    h.probe.setSurface('session', 'Busytools/forge/embed-b-plan');
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames[0]?.session, 'the gap happened on the lead seat').toBe('Busytools/forge/lead');
  });

  /** **since-beat is absent before the first tick**: there is no arrival to
   *  measure from, and a zero would invent one. */
  it('leaves since-beat absent before the first tick lands', () => {
    const h = harness();
    h.frame(16);
    h.frame(5000); // no beat has ever arrived
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames[0]?.worker_beats).toBe(0);
    expect(frames[0], 'no arrival to measure from').not.toHaveProperty('worker_since_beat_ms');
  });

  /**
   * **The suspended shape** (#2374): a page that resumes fires its ONE
   * overdue interval, so a long gap carries a single beat with the expected
   * count far above it - the reading that tells "suspended" from "ran".
   */
  it('says a resumed page carried one beat where twenty were due', () => {
    const h = harness();
    h.frame(16);
    h.frame(20_000);
    h.beat(0); // the overdue interval, fired once on resume
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames[0]?.worker_beats, 'one beat').toBe(1);
    expect(
      frames[0]?.worker_expected_beats,
      'against twenty due - the suspended shape, not a running page',
    ).toBe(20);
  });

  /**
   * **Zero beats says nothing over a short gap** (#2374): at one tick a
   * second, a gap shorter than a tick carries no beat by construction. The
   * record carries the time since the last arrival so a reader can tell
   * "nothing was due" from a page that stopped ticking - and the high
   * lateness of the previous frame must NOT leak into this one.
   */
  it('reports a zero-beat gap with its since-beat time, and no stale lateness', () => {
    const h = harness();
    h.frame(16); // baseline at 1016
    h.beat(1200); // the last arrival, at 1016
    h.frame(800); // this one carries the lateness…
    h.settle();

    h.frame(150); // …and this one lands 950 ms after the last beat: nothing due yet
    h.settle();

    const frames = h.records.flat().filter((record) => record.kind === 'frame');
    expect(frames).toHaveLength(2);
    expect(frames[1]?.worker_beats, 'no tick was due').toBe(0);
    expect(frames[1], 'a zero-beat frame invents no lateness').not.toHaveProperty(
      'worker_max_late_ms',
    );
    expect(frames[1]?.worker_since_beat_ms, 'and says how long since the last arrival').toBe(950);
  });

  /**
   * **A page with no Web Worker writes no evidence fields, rather than
   * zero.** This exercises the REAL port: this test environment has no
   * `Worker`, so the production path answers null and the fields stay
   * absent.
   */
  it('leaves the heartbeat fields absent where there is no worker', () => {
    const records: PerfRecord[][] = [];
    let clock = 1_000;
    const callbacks: (() => void)[] = [];
    const deferred: (() => void)[] = [];
    const probe = perfProbe({
      now: () => clock,
      schedule: (cb) => {
        callbacks.push(cb);
        return () => undefined;
      },
      observeLongTasks: () => () => undefined,
      defer: (cb) => {
        deferred.push(cb);
      },
      sink: (batch) => {
        records.push(batch);
      },
    });
    probe.start();
    const frame = (ms: number) => {
      clock += ms;
      callbacks.shift()?.();
    };
    frame(16);
    frame(5000);
    for (let cb = deferred.shift(); cb !== undefined; cb = deferred.shift()) cb();

    const started = records.flat().find((record) => record.kind === 'run_started');
    expect(started?.heartbeat, 'the run line says the page has no heartbeat').toBe(false);
    const slow = records.flat().filter((record) => record.kind === 'frame');
    expect(slow).toHaveLength(1);
    expect(slow[0]).not.toHaveProperty('worker_beats');
    expect(slow[0]).not.toHaveProperty('worker_max_late_ms');
    probe.stop();
  });

  /**
   * **A worker that fails to load is announced, never silent** (#2374): the
   * load errors arrive on the worker, not at the constructor, so the port
   * reports them - naming the event's own error - and the fields go ABSENT,
   * because a broken asset must not read as a page that stopped. One
   * record, once.
   */
  it('announces a worker that failed to load and omits the fields after it', () => {
    const h = harness();
    h.frame(16);
    h.failHeartbeat('SecurityError: blocked by the scheme');
    h.failHeartbeat('second error, another one is not a record');

    h.beat(5); // a beat that still somehow arrives must not revive the fields
    h.frame(5000);
    h.settle();

    const beats = h.records.flat().filter((record) => record.kind === 'heartbeat');
    expect(beats).toHaveLength(1);
    expect(beats[0]?.heartbeat, 'the record names the failure').toBe(false);
    expect(beats[0]?.name, "and carries the event's own error").toContain('SecurityError');
    const slow = h.records.flat().filter((record) => record.kind === 'frame');
    expect(slow[0], 'and the frame carries no beat evidence after it').not.toHaveProperty(
      'worker_beats',
    );
  });

  /** The run line says whether the page has a heartbeat at all. */
  it('says on the run line that a heartbeat is running', () => {
    const h = harness();
    const started = h.records.flat().find((record) => record.kind === 'run_started');
    expect(started?.heartbeat).toBe(true);
    expect(started?.worker_tick_ms).toBe(1000);
  });

  /** Stopping the probe disconnects the heartbeat with everything else. */
  it('stops the heartbeat where it was told to', () => {
    const h = harness();
    expect(h.beating).toBe(true);
    h.probe.stop();
    expect(h.beating, 'the worker is torn down with the probe').toBe(false);
  });

  /**
   * **The REAL port, below its no-Worker guard** (#2374): the engine's
   * worker is stubbed, so the listeners, the min-offset lateness and both
   * failure paths run as shipped. Two ticks 500 ms apart must report a
   * 500 ms worst lateness - a last-wins offset would report zero - and the
   * teardown must terminate the worker and drop its listeners.
   */
  it('runs the real port: min-offset lateness, and the worker torn down', () => {
    class FakeWorker {
      static last: FakeWorker | null = null;
      readonly listeners = new Map<string, Set<(event: unknown) => void>>();
      readonly terminate = vi.fn();
      constructor() {
        FakeWorker.last = this;
      }
      addEventListener(type: string, fn: (event: unknown) => void): void {
        const set = this.listeners.get(type) ?? new Set<(event: unknown) => void>();
        set.add(fn);
        this.listeners.set(type, set);
      }
      removeEventListener(type: string, fn: (event: unknown) => void): void {
        this.listeners.get(type)?.delete(fn);
      }
      emit(type: string, event: unknown): void {
        for (const fn of this.listeners.get(type) ?? []) fn(event);
      }
    }
    vi.stubGlobal('Worker', FakeWorker);
    let clock = 1_000;
    const nowSpy = vi.spyOn(performance, 'now').mockImplementation(() => clock);
    try {
      const records: PerfRecord[][] = [];
      const callbacks: (() => void)[] = [];
      const deferred: (() => void)[] = [];
      const probe = perfProbe({
        now: () => clock,
        schedule: (cb) => {
          callbacks.push(cb);
          return () => undefined;
        },
        observeLongTasks: () => () => undefined,
        defer: (cb) => {
          deferred.push(cb);
        },
        sink: (batch) => {
          records.push(batch);
        },
      });
      probe.start();
      const worker = FakeWorker.last;
      expect(worker, 'the real port constructed a Worker').not.toBeNull();
      const frame = (ms: number) => {
        clock += ms;
        callbacks.shift()?.();
      };
      frame(16); // the baseline frame
      // Two ticks as the engine sends them: the same worker stamp, the
      // second arriving 500 ms later.
      worker?.emit('message', { data: { t: 100 } }); // arrival 1_016: raw 916
      clock += 500;
      worker?.emit('message', { data: { t: 100 } }); // arrival 1_516: raw 1_416
      frame(5000);
      for (let cb = deferred.shift(); cb !== undefined; cb = deferred.shift()) cb();

      const slow = records.flat().filter((record) => record.kind === 'frame');
      expect(slow[0]?.worker_beats).toBe(2);
      expect(
        slow[0]?.worker_max_late_ms,
        'the second tick is 500 ms later than the first: min-offset keeps the first as zero',
      ).toBe(500);
      expect(slow[0]?.worker_expected_beats).toBe(5);

      probe.stop();
      expect(worker?.terminate, 'the worker is terminated on stop').toHaveBeenCalled();
      expect(worker?.listeners.get('message')?.size ?? 0, 'and its listeners are removed').toBe(0);
      expect(worker?.listeners.get('error')?.size ?? 0, 'the error listener goes with them').toBe(
        0,
      );
      expect(
        worker?.listeners.get('messageerror')?.size ?? 0,
        'and so does the messageerror one',
      ).toBe(0);
    } finally {
      nowSpy.mockRestore();
      vi.unstubAllGlobals();
    }
  });

  /**
   * **Every load-error shape announces, and the cause is read where the DOM
   * puts it** (#2374): a script error is an `ErrorEvent` whose name and
   * message sit on `error`; `messageerror` is a `MessageEvent` carrying
   * neither; a fetch or MIME failure is a plain `Event` with only its type.
   * One record per run, because one failure ends the heartbeat for the page.
   */
  it('announces a worker load failure for every real error shape', () => {
    class FailingWorker {
      static last: FailingWorker | null = null;
      readonly listeners = new Map<string, Set<(event: unknown) => void>>();
      readonly terminate = vi.fn();
      constructor() {
        FailingWorker.last = this;
      }
      addEventListener(type: string, fn: (event: unknown) => void): void {
        const set = this.listeners.get(type) ?? new Set<(event: unknown) => void>();
        set.add(fn);
        this.listeners.set(type, set);
      }
      removeEventListener(type: string, fn: (event: unknown) => void): void {
        this.listeners.get(type)?.delete(fn);
      }
      emit(type: string, event: unknown): void {
        for (const fn of this.listeners.get(type) ?? []) fn(event);
      }
    }
    vi.stubGlobal('Worker', FailingWorker);
    const cases: [string, unknown, string][] = [
      // A script error: ErrorEvent, the cause on `error`.
      [
        'error',
        { type: 'error', error: { name: 'SecurityError', message: 'blocked' } },
        'SecurityError',
      ],
      // No error payload: the event's own type names it.
      ['error', { type: 'error' }, 'error'],
      // A message that could not be deserialized: MessageEvent, neither field.
      ['messageerror', { type: 'messageerror' }, 'messageerror'],
    ];
    try {
      for (const [path, event, named] of cases) {
        const records: PerfRecord[][] = [];
        const probe = perfProbe({
          schedule: () => () => undefined,
          observeLongTasks: () => () => undefined,
          sink: (batch) => {
            records.push(batch);
          },
        });
        probe.start();
        FailingWorker.last?.emit(path, event);
        const failures = records.flat().filter((record) => record.kind === 'heartbeat');
        expect(failures, `${path} is announced`).toHaveLength(1);
        expect(failures[0]?.name, `the cause named for ${path}`).toContain(named);
        probe.stop();
        FailingWorker.last = null;
      }
    } finally {
      vi.unstubAllGlobals();
    }
  });

  /**
   * **A construction throw is a FAILURE, not an absent worker** (#2374): a
   * page that could not build its worker must read differently from one
   * that has none, so the record names it and the run line still says no
   * heartbeat. The report happens synchronously, before the port answers
   * null, which is why the failure flag is not the heartbeat flag.
   */
  it('announces a worker whose construction throws', () => {
    class ThrowingWorker {
      constructor() {
        throw new Error('no worker here');
      }
    }
    vi.stubGlobal('Worker', ThrowingWorker);
    try {
      const records: PerfRecord[][] = [];
      const probe = perfProbe({
        schedule: () => () => undefined,
        observeLongTasks: () => () => undefined,
        sink: (batch) => {
          records.push(batch);
        },
      });
      probe.start();
      const failures = records.flat().filter((record) => record.kind === 'heartbeat');
      expect(failures, 'one record names the construction failure').toHaveLength(1);
      expect(failures[0]?.name).toContain('constructor');
      const started = records.flat().find((record) => record.kind === 'run_started');
      expect(started?.heartbeat, 'and the run line says no heartbeat').toBe(false);
      probe.stop();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

describe('the open a conversation begins', () => {
  it('marks the open, then measures the drawn and the quiet from it', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    const switches = h.records.flat().filter((record) => record.kind === 'switch');
    expect(switches).toHaveLength(1);
    expect(switches[0]?.session).toBe('Busytools/forge/lead');
    expect(switches[0]?.surface).toBe('session');

    // A normal frame while the seat's answer is still on its way must not
    // read as the open being done: the quiet needs the drawing first.
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);

    h.probe.noteDrawn('conversation');
    const drawn = h.records.flat().filter((record) => record.kind === 'drawn');
    expect(drawn).toHaveLength(1);
    expect(drawn[0]?.ms, 'the drawn mark measures from the open').toBe(16);
    expect(drawn[0]?.state, 'and says what completed the open').toBe('conversation');

    // A slow frame after the drawing is the open still being paid for, so it
    // stays running.
    h.frame(120);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);

    // The next normal frame is the quiet, measured from the same open.
    h.frame(16);
    const quiet = h.records.flat().filter((record) => record.kind === 'quiet');
    expect(quiet).toHaveLength(1);
    expect(quiet[0]?.ms).toBe(152);

    // A settled column writes nothing more for this open.
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(1);
  });

  /**
   * **A route change is not a conversation.** The board route names an
   * org/project and no seat, and the review, files and doc routes name a
   * seat no conversation opens for: an open marked off the route would write
   * switches that never settle, or that steal the next real open.
   */
  it('marks nothing for a route change alone', () => {
    const h = harness();
    h.probe.setSurface('board', 'Busytools/forge');
    h.probe.noteDrawn('conversation');
    h.frame(16);
    h.frame(120);

    const kinds = h.records.flat().map((record) => record.kind);
    expect(kinds, 'no open, no drawing and no quiet come from a route move').not.toContain(
      'switch',
    );
    expect(kinds).not.toContain('drawn');
    expect(kinds).not.toContain('quiet');
  });

  it('marks a reopen of the same seat: a reconnect opens its conversation again', () => {
    const h = harness();
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    expect(h.records.flat().filter((record) => record.kind === 'switch')).toHaveLength(2);
  });

  /**
   * **One drawn mark per open, and none after the open has settled.** The
   * dedupe lives in the probe, and removing it is silent: the column draws
   * on every update it holds, so each redraw would write its own mark and
   * the log would read as a burst of opens that never happened.
   */
  it('writes one drawn mark per open, whatever the column redraws', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.frame(16);

    h.probe.noteDrawn('conversation');
    h.probe.noteDrawn('conversation');
    h.frame(16);
    h.probe.noteDrawn('conversation'); // the open already settled: a stray redraw marks nothing

    expect(h.records.flat().filter((record) => record.kind === 'drawn')).toHaveLength(1);
  });

  /**
   * **A placeholder does not end the open** (#2374): a waking line is the
   * column saying the seat is coming up, not its content arriving. The open
   * stays armed, no quiet may fire while the content is still on its way,
   * and the content's own drawing follows with the total from the switch -
   * which is the click-to-first-row measurement this cut exists for.
   */
  it('records a waking placeholder, then the content drawing, then the quiet', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.frame(16);

    h.probe.noteDrawn('waking');
    h.probe.noteDrawn('waking'); // a redraw of the placeholder marks nothing new
    // Frames while the content is still on its way settle nothing.
    h.frame(16);
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);
    expect(h.records.flat().filter((record) => record.kind === 'drawn')).toHaveLength(1);

    // The content lands; its drawing carries the whole time since the switch.
    h.frame(120); // the arrival's own slow frame
    h.probe.noteDrawn('conversation');
    h.frame(16);

    const drawn = h.records.flat().filter((record) => record.kind === 'drawn');
    expect(drawn).toHaveLength(2);
    expect(drawn[0]?.state).toBe('waking');
    expect(drawn[0]?.ms).toBe(16);
    expect(drawn[1]?.state).toBe('conversation');
    expect(
      drawn[1]?.ms,
      'the content drawing measures from the switch, placeholder or no placeholder',
    ).toBe(168);
    const quiet = h.records.flat().filter((record) => record.kind === 'quiet');
    expect(quiet).toHaveLength(1);
    expect(quiet[0]?.ms, 'and the quiet settles the same open').toBe(184);
  });

  /** The refused placeholder is the same shape: an answer, but not content. */
  it('records a refused placeholder, then the content and its quiet', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.frame(16);

    h.probe.noteDrawn('refused');
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);

    h.probe.noteDrawn('conversation');
    h.frame(16);

    const drawn = h.records.flat().filter((record) => record.kind === 'drawn');
    expect(drawn.map((record) => record.state)).toEqual(['refused', 'conversation']);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(1);
  });

  /** An empty drawing is content, so the quiet follows it. */
  it('lets the quiet follow an empty drawing', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.frame(16);

    h.probe.noteDrawn('empty');
    h.frame(16);

    const drawn = h.records.flat().filter((record) => record.kind === 'drawn');
    expect(drawn[0]?.state, 'the state crosses verbatim, not mapped').toBe('empty');
    const quiet = h.records.flat().filter((record) => record.kind === 'quiet');
    expect(quiet).toHaveLength(1);
    expect(quiet[0]?.ms).toBe(32);
  });

  /**
   * **A switch after one conversation has drawn is a new open** (#2374).
   * The flags belong to the open: a reset left out of
   * `noteConversationOpened` would leave every later seat measuring nothing,
   * so this opens a second conversation on top of a settled first one and
   * wants both drawings and both quiets.
   */
  it('measures a second conversation opened after one has drawn', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.frame(16);
    h.probe.noteDrawn('conversation');
    h.frame(16); // the first open settles

    h.probe.setSurface('session', 'Busytools/forge/w1');
    h.probe.noteConversationOpened('Busytools/forge/w1');
    h.frame(16);
    h.probe.noteDrawn('conversation');
    h.frame(16);

    const seats = (kind: string) =>
      h.records
        .flat()
        .filter((record) => record.kind === kind)
        .map((record) => record.session);
    expect(seats('drawn'), 'the second seat draws its own mark').toEqual([
      'Busytools/forge/lead',
      'Busytools/forge/w1',
    ]);
    expect(seats('quiet'), 'and settles on its own quiet').toEqual([
      'Busytools/forge/lead',
      'Busytools/forge/w1',
    ]);
  });

  /**
   * **A mark names the seat that opened, not where the page moved to.** The
   * route can move on while the seat's drawing is still on its way - to a
   * view of the same seat, whose whole point is that it is not a switch.
   */
  it('attributes the marks to the seat that opened, not the live route', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.probe.setSurface('review', 'Busytools/forge/lead');
    h.frame(16); // a normal frame while the seat's answer is still on its way

    h.probe.noteDrawn('conversation');
    h.frame(16);

    const marks = h.records
      .flat()
      .filter((record) => record.kind === 'drawn' || record.kind === 'quiet');
    expect(marks).toHaveLength(2);
    for (const mark of marks) {
      expect(mark.session, 'the seat itself is what the mark names').toBe('Busytools/forge/lead');
      expect(mark.surface).toBe('session');
    }
    expect(marks[1]?.ms, 'the quiet measures from the open too').toBe(32);
  });

  /**
   * **An open the page leaves is dropped, not carried.** Home between the
   * open and its first normal frame would otherwise write the seat's own
   * delta under a route that never opened anything.
   */
  it('drops an unfinished open when the page leaves its seat', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.noteConversationOpened('Busytools/forge/lead');
    h.probe.setSurface('home', null);

    h.probe.noteDrawn('conversation');
    h.frame(16);
    h.frame(16);

    const kinds = h.records.flat().map((record) => record.kind);
    expect(kinds, 'a dropped open leaves no late drawn mark').not.toContain('drawn');
    expect(kinds, 'and no quiet under the route that came next').not.toContain('quiet');
  });
});

describe('the surface a record names', () => {
  it('names the seat on a seat route', () => {
    expect(
      perfSurface({ name: 'session', slot: { org: 'Busytools', project: 'forge', label: 'lead' } }),
    ).toEqual({ surface: 'session', session: 'Busytools/forge/lead' });
  });

  /**
   * **One builder, one spelling.** The surface a frame carries and the seat
   * an open's marks name are paired by this string, so a second hand-rolled
   * join is exactly how that pairing silently stops finding anything.
   */
  it('names a seat the same way for a frame and for an open', () => {
    const slot = { org: 'Busytools', project: 'forge', label: 'lead' };
    expect(seatLabel(slot)).toBe('Busytools/forge/lead');
    expect(perfSurface({ name: 'session', slot }).session).toBe(seatLabel(slot));
  });

  it('names the project on a board', () => {
    expect(perfSurface({ name: 'board', org: 'Busytools', project: 'forge' })).toEqual({
      surface: 'board',
      session: 'Busytools/forge',
    });
  });

  it('leaves a sessionless route without one', () => {
    expect(perfSurface({ name: 'home' })).toEqual({ surface: 'home', session: null });
  });
});

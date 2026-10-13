import { describe, expect, it } from 'vitest';

import { perfProbe, perfSurface, startPerfProbe, type PerfRecord } from './perf';

/**
 * The probe's clock and frame source, driven by the test.
 *
 * The core is injected with these rather than reaching for `performance` and
 * `requestAnimationFrame` itself, which is the whole reason a known slow
 * frame can be produced here instead of hoped for on a machine.
 */
function harness(
  options: {
    summaryIntervalMs?: number;
    surface?: string | null;
    session?: string | null;
  } = {},
) {
  const records: PerfRecord[][] = [];
  let clock = 1_000;
  const callbacks: (() => void)[] = [];
  let longTask: ((ms: number, name: string) => void) | null = null;
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
    sink: (batch) => {
      records.push(batch);
    },
    ...(options.summaryIntervalMs === undefined
      ? {}
      : { summaryIntervalMs: options.summaryIntervalMs }),
  });
  // Boot's own reading, before the probe opens its first window - the order
  // main.ts has (the probe starts, the router wires the route later).
  if (options.surface !== undefined) probe.setSurface(options.surface, options.session ?? null);
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
    get stopped() {
      return stopped;
    },
  };
}

describe('the frame probe', () => {
  it('opens its log with a run line', () => {
    const h = harness({ surface: 'connect', session: null });
    const first = h.records.flat()[0];
    expect(first?.kind).toBe('run_started');
    expect(first?.schema).toBe('forge-perf/v1');
    expect(first?.run_id).toBeTruthy();
    expect(first?.version).toBeTruthy();
    // Boot's own reading rides along, so even the run line is attributable.
    expect(first?.surface).toBe('connect');
    expect(first?.session).toBeNull();
  });

  it('records a slow frame and nothing for the fast ones between', () => {
    const h = harness();
    h.probe.setSurface('session', 'forge/lead');
    h.frame(16);
    h.frame(16);
    h.frame(120);

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
    // the terminal's own count-then-check rule.
    expect(summaries).toHaveLength(1);
    expect(summaries[0]?.frames).toBe(3);
    expect(summaries[0]?.slow).toBe(1);
    expect(summaries[0]?.max_ms).toBe(200);
  });

  it('names the window it summarises, and leaves frames their own place', () => {
    const h = harness({ summaryIntervalMs: 100, surface: 'home', session: null });
    h.frame(16);
    h.frame(16);
    h.frame(200);

    const summary = h.records.flat().find((record) => record.kind === 'summary');
    expect(summary?.surface, 'the summary says where its window opened').toBe('home');

    // A seat switch mid-window: the next frame carries where IT happened
    // (the 200 ms crossing frame above is its own record, under `home`).
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.frame(120);
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
});

describe('the surface a record names', () => {
  it('names the seat on a seat route', () => {
    expect(
      perfSurface({ name: 'session', slot: { org: 'Busytools', project: 'forge', label: 'lead' } }),
    ).toEqual({ surface: 'session', session: 'Busytools/forge/lead' });
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

import { describe, expect, it } from 'vitest';

import { perfProbe, perfSurface, startPerfProbe, type PerfRecord } from './perf';

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
    const frame = h.records.flat().find((record) => record.kind === 'frame');
    expect(frame?.surface).toBe('connect');
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
});

describe('the open a seat switch begins', () => {
  it('marks the switch, then measures the drawn and the quiet from it', () => {
    const h = harness();
    // Boot reads the route first: that wiring is the run line's own moment,
    // not a switch.
    h.probe.setSurface('home', null);
    h.frame(16);

    h.probe.setSurface('session', 'Busytools/forge/lead');
    const switches = h.records.flat().filter((record) => record.kind === 'switch');
    expect(switches).toHaveLength(1);
    expect(switches[0]?.session).toBe('Busytools/forge/lead');

    // A normal frame while the seat's answer is still on its way must not
    // read as the open being done: the quiet needs the drawing first.
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);

    h.probe.noteDrawn();
    const drawn = h.records.flat().filter((record) => record.kind === 'drawn');
    expect(drawn).toHaveLength(1);
    expect(drawn[0]?.ms, 'the drawn mark measures from the switch').toBe(16);

    // A slow frame after the drawing is the switch still being paid for, so
    // the open stays running.
    h.frame(120);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(0);

    // The next normal frame is the quiet, measured from the same switch.
    h.frame(16);
    const quiet = h.records.flat().filter((record) => record.kind === 'quiet');
    expect(quiet).toHaveLength(1);
    expect(quiet[0]?.ms).toBe(152);

    // A settled column writes nothing more for this open.
    h.frame(16);
    expect(h.records.flat().filter((record) => record.kind === 'quiet')).toHaveLength(1);
  });

  it('does not mark booting straight onto a seat as a switch', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    expect(h.records.flat().filter((record) => record.kind === 'switch')).toHaveLength(0);
  });

  it('does not mark moving between views of the same seat', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.setSurface('review', 'Busytools/forge/lead');
    expect(h.records.flat().filter((record) => record.kind === 'switch')).toHaveLength(0);
  });

  it('marks a return to a seat as a switch, and does not mark the home it left through', () => {
    const h = harness();
    h.probe.setSurface('session', 'Busytools/forge/lead');
    h.probe.setSurface('home', null);
    expect(h.records.flat().filter((record) => record.kind === 'switch')).toHaveLength(0);
    h.probe.setSurface('session', 'Busytools/forge/lead');
    const switches = h.records.flat().filter((record) => record.kind === 'switch');
    expect(switches).toHaveLength(1);
    expect(switches[0]?.surface).toBe('session');
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

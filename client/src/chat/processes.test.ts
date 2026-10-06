import { describe, expect, it } from 'vitest';

import type { BackgroundTask, ProcessSnapshot } from '../session/wire';
import { Processes } from './processes.svelte';

const walk = (processes: ProcessSnapshot['processes'], tick = 0): ProcessSnapshot => ({
  processes,
  scanned_at: { secs_since_epoch: tick, nanos_since_epoch: 0 },
});

const registryRow = (over: Partial<BackgroundTask> = {}): BackgroundTask => ({
  task_id: 't-1',
  task_type: 'local_bash',
  description: 'Run the full suite',
  command: 'cargo nextest run',
  tool_use_id: null,
  ...over,
});

describe('the batch rows', () => {
  it('adopts the process its command names, so the row carries memory and pid', () => {
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 8842,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run -p forge-web',
          memory_bytes: 512,
        },
      ]),
      [registryRow({ tool_use_id: 'tu-1' })],
      true,
    );

    const rows = store.rows();

    expect(rows, 'one call, one row').toHaveLength(1);
    expect(
      rows[0],
      'the description leads, the walk supplies the figure, and the call rides along',
    ).toMatchObject({
      headline: 'Run the full suite',
      memory_bytes: 512,
      pid: 8842,
      tool_use_id: 'tu-1',
    });
  });

  it('draws a call the walk missed, without a figure, under the measured', () => {
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 8842,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run',
          memory_bytes: 512,
        },
      ]),
      [
        registryRow({
          task_id: 't-2',
          command: 'setsid node watch.js',
          description: 'Watch flags',
        }),
        registryRow(),
      ],
      true,
    );

    const rows = store.rows();

    expect(rows, 'both calls draw').toHaveLength(2);
    expect(rows[0], 'the matched one carries its figure').toMatchObject({
      headline: 'Run the full suite',
      memory_bytes: 512,
    });
    // A row nothing measured sorts last and carries no figure: an absent
    // measurement is not a measurement of zero.
    expect(rows[1], 'the unmeasured row is last and figures nothing').toMatchObject({
      headline: 'Watch flags',
      memory_bytes: null,
      pid: null,
    });
  });

  it('never draws a walk process no call names - the row is batch work only', () => {
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 4401,
          parent_pid: 1,
          name: 'node',
          command: 'node /opt/mcp-servers/forge-server.js',
          memory_bytes: 96_000_000,
        },
        {
          pid: 4452,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run -p forge-server',
          memory_bytes: 400_000_000,
        },
      ]),
      [registryRow({ command: 'cargo nextest run', task_id: 't-run' })],
      true,
    );

    const rows = store.rows();

    expect(rows, 'only the registry relation draws').toHaveLength(1);
    expect(rows[0]?.pid, 'and it is the process the call names').toBe(4452);
    expect(
      rows.some((row) => row.pid === 4401),
      'the MCP server process does not appear',
    ).toBe(false);
  });

  it('keeps a call that settles until the turn closes, then cleans it', () => {
    const store = new Processes();
    store.sync(walk([]), [registryRow({ tool_use_id: 'tu-1' })], true);
    expect(store.running(), 'running while the registry holds it').toBe(1);

    // The task finishes: the registry arrives whole and no longer names it,
    // while its verdict has not arrived yet - the row must draw without one,
    // no cross and no invented end time.
    store.sync(walk([]), [], true);
    const bare = store.rows();
    expect(bare, 'the settled row is kept for the turn').toHaveLength(1);
    expect(bare[0], 'with its words and nothing it does not know').toMatchObject({
      headline: 'Run the full suite',
      settled: true,
      failed: false,
      ended_ms: null,
      memory_bytes: null,
      tool_use_id: 'tu-1',
    });
    expect(store.running(), 'and it no longer counts as running').toBe(0);

    // The verdict arrives a sync later and lands on the row already held.
    store.sync(walk([]), [], true, {
      calls: new Map([['tu-1', { failed: true, ended_ms: 1_700_000_000_000 }]]),
      owners: new Map(),
    });
    expect(store.rows()[0], 'the late outcome reaches the settled row').toMatchObject({
      failed: true,
      ended_ms: 1_700_000_000_000,
    });

    // The turn closes on a registry that EMPTIES with it: the cleaning must
    // not double as detection, or everything that left with the close lands
    // back in the settled list - the exact shape the close fix exists for.
    store.sync(walk([]), [registryRow({ task_id: 't-2' })], true);
    expect(store.running(), 'a second call runs into the close').toBe(1);
    store.sync(walk([]), [], false);
    expect(store.rows(), 'the closing sync cleans without re-settling').toHaveLength(0);
    expect(store.anything(), 'and an empty row draws nothing').toBe(false);
  });

  it('never settles an agent task: the agents row owns its own life', () => {
    // #settled's kind filter is a second gate, not a redundant one: the
    // settled list never passes through #batches, so without it an agent
    // leaving the registry would draw in BOTH strip rows.
    const store = new Processes();
    store.sync(walk([]), [registryRow({ task_id: 't-agent', task_type: 'local_agent' })], true);
    store.sync(walk([]), [], true);

    expect(store.rows(), 'the agent task left without settling here').toHaveLength(0);
    expect(store.anything(), 'and it draws nothing here either').toBe(false);
  });

  it("adopts a quoted command's own wrapper, the terminal's rule wired in", () => {
    // The finding's worked case end to end: the registry command carries
    // plain quotes, the scanned cmdline carries the wrapper's escape, and the
    // row must still wear the figure.
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 700,
          parent_pid: 1,
          name: 'zsh',
          command:
            "/bin/zsh -c source /x/snap.sh 2>/dev/null || true && eval 'git commit -m '\"'\"'fix auth timeout'\"'\"'' < /dev/null && pwd -P >| /tmp/claude-x-cwd",
          memory_bytes: 2048,
        },
      ]),
      [registryRow({ command: "git commit -m 'fix auth timeout'" })],
      true,
    );

    const rows = store.rows();
    expect(rows, 'one call').toHaveLength(1);
    expect(rows[0]?.memory_bytes, 'the quoted command still finds its process').toBe(2048);
    expect(rows[0]?.pid, 'adopted by that very process').toBe(700);
  });

  it('totals the running calls, a settled row excluded', () => {
    const store = new Processes();
    const pair = (suiteTick: number) =>
      walk(
        [
          {
            pid: 1,
            parent_pid: 0,
            name: 'cargo',
            command: 'cargo nextest run',
            memory_bytes: 512,
          },
          {
            pid: 2,
            parent_pid: 0,
            name: 'tail',
            command: 'tail -f watch.log',
            memory_bytes: 64,
          },
        ],
        suiteTick,
      );
    const rows = [
      registryRow({ task_id: 't-a', command: 'cargo nextest run' }),
      registryRow({ task_id: 't-b', command: 'tail -f watch.log' }),
    ];
    store.sync(pair(1), rows, true);
    expect(store.totalBytes(), 'both running families together').toBe(576);

    store.sync(pair(2), [rows[0] as BackgroundTask], true);
    expect(store.totalBytes(), "a settled call's memory is no longer consumed").toBe(512);
  });

  it("finds a row's call in the task_started link when its own id is null", () => {
    // The wire order is announce-then-link: the registry snapshot the client
    // holds predates the frame that names the call, so the row's own id is
    // null and the reveal would answer nothing without this join - the
    // reproduced "click registered, nothing happens".
    const store = new Processes();
    store.sync(walk([]), [registryRow({ tool_use_id: null })], true, {
      calls: new Map(),
      owners: new Map([['t-1', 'tu-linked']]),
    });

    const rows = store.rows();

    expect(rows[0]?.tool_use_id, 'the link fills the row').toBe('tu-linked');
  });

  it("counts the matched process's whole family, not its own line", () => {
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 4210,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run',
          memory_bytes: 400,
        },
        {
          pid: 4211,
          parent_pid: 4210,
          name: 'rustc',
          command: 'rustc --crate-name a',
          memory_bytes: 310,
        },
        {
          pid: 4212,
          parent_pid: 4210,
          name: 'sccache',
          command: 'sccache --start-server',
          memory_bytes: 62,
        },
        { pid: 4300, parent_pid: 1, name: 'node', command: 'node other.js', memory_bytes: 999 },
      ]),
      [registryRow({ command: 'cargo nextest run', tool_use_id: 'tu-1' })],
      true,
    );

    const rows = store.rows();

    expect(rows, 'one call').toHaveLength(1);
    expect(rows[0]?.memory_bytes, 'the family total, not the own line, and not the stranger').toBe(
      772,
    );
  });

  it('never draws a non-batch kind: an agent belongs to the agents row', () => {
    const store = new Processes();
    store.sync(
      walk([]),
      [
        registryRow({ task_id: 't-agent', task_type: 'local_agent', description: 'audit a thing' }),
        registryRow({
          task_id: 't-wf',
          task_type: 'local_workflow',
          description: 'run a workflow',
        }),
        registryRow({ task_id: 't-bash' }),
      ],
      true,
    );

    const rows = store.rows();

    expect(rows, 'only the bash call draws here').toHaveLength(1);
    expect(rows[0]?.kind, 'and the kind is the one that draws here').toBe('local_bash');
    expect(store.running(), 'and the count agrees with the list').toBe(1);
  });

  it('gives one registry row to one process, so a shared command adopts once', () => {
    const store = new Processes();
    store.sync(
      walk([
        {
          pid: 1,
          parent_pid: 0,
          name: 'cargo',
          command: 'cargo nextest run -p forge-web',
          memory_bytes: 900,
        },
        {
          pid: 2,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run --release',
          memory_bytes: 100,
        },
      ]),
      [
        registryRow({ task_id: 't-a' }),
        registryRow({ task_id: 't-b', description: 'Run the suite again' }),
      ],
      true,
    );

    const rows = store.rows();

    expect(rows, 'both calls draw').toHaveLength(2);
    expect(rows[0]?.pid, 'each adopts its own process').not.toBe(rows[1]?.pid);
    const pids = rows.map((row) => row.pid).sort();
    expect(pids, 'and between them they take both processes').toEqual([1, 2]);
  });
});

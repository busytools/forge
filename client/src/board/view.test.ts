import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus, TaskLinkWire } from '../wire/home';
import { boardView, fmtSecs } from './view';

function task(id: string, subject: string, status: TaskStatus, over: Partial<Task> = {}): Task {
  return {
    id,
    project_name: 'proj',
    subject,
    active_form: null,
    detail: null,
    status,
    owner: null,
    parent: null,
    waiting_on: null,
    estimate: null,
    rank: null,
    verify: null,
    links: [],
    attempt: 0,
    archived_at: null,
    created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    ...over,
  };
}

function marks(over: Partial<Marks> = {}): Marks {
  return {
    ready: false,
    in_review: false,
    overdue: false,
    no_movement: false,
    waiting_too_long: false,
    stale: false,
    to_close: false,
    ...over,
  };
}

function row(task: Task, over: Partial<BoardRow> = {}): BoardRow {
  return {
    task,
    worked_secs: 0,
    updated_secs_ago: 0,
    marks: marks(),
    rollup: null,
    parent_subject: null,
    ...over,
  };
}

function wireWith(rows: BoardRow[]): HomeWire {
  const first = homeWire.projects[0];
  if (first === undefined) throw new Error('the fixture holds no project');
  return { ...homeWire, projects: [{ ...first, rows }] };
}

describe("one project's board", () => {
  /**
   * Two levels, in the queue's own order: rank first, then creation, so an
   * unranked row sorts after the ranked ones - the order a lead dispatches
   * from is the order the board shows.
   */
  it('orders epics by rank and nests their children under them', () => {
    const wire = wireWith([
      row(task('late', 'late epic', 'pending', { rank: 5 })),
      row(task('early', 'early epic', 'pending', { rank: 1 })),
      row(task('child', 'a child', 'in_progress', { parent: 'early' })),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.rows.map((entry) => entry.id)).toEqual(['early', 'child', 'late']);
    expect(view.rows[0]?.depth).toBe(0);
    expect(view.rows[1]?.depth).toBe(1);
  });

  /**
   * The waiting section carries the rows that wait on the READER - a
   * verification and a question alike - with the facts the action needs.
   */
  it('gathers the rows waiting on the reader', () => {
    const wire = wireWith([
      row(
        task('verify', 'a look', 'waiting', {
          waiting_on: { kind: 'decision', detail: 'the mock', on: null, verification: true },
        }),
        { updated_secs_ago: 2_400 },
      ),
      row(
        task('ask', 'a question', 'waiting', {
          waiting_on: { kind: 'decision', detail: 'which shape', on: null, verification: false },
        }),
      ),
      row(
        task('dep', 'a dependency', 'waiting', {
          waiting_on: { kind: 'dependency', detail: null, on: 'other', verification: false },
        }),
      ),
      row(task('run', 'running', 'in_progress')),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.waiting.map((entry) => entry.id)).toEqual(['verify', 'ask']);
    expect(view.waiting[0]?.verification).toBe(true);
    expect(view.waiting[0]?.age).toBe('40m');
    expect(view.waiting[1]?.verification).toBe(false);
  });

  /** The marks become words, and the wait's kind decides the chip. */
  it('words the marks and the wait', () => {
    const wire = wireWith([
      row(
        task('t1', 'over', 'in_progress', {
          estimate: { words: '1d', secs: 86_400 },
        }),
        { worked_secs: 90_000, updated_secs_ago: 7_200, marks: marks({ overdue: true }) },
      ),
      row(
        task('t2', 'waits', 'waiting', {
          waiting_on: { kind: 'resource', detail: null, on: null, verification: false },
        }),
        { updated_secs_ago: 18_000, marks: marks({ waiting_too_long: true }) },
      ),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    const over = view.rows.find((entry) => entry.id === 't1');
    expect(over?.worked).toBe('25h');
    expect(over?.estimate).toBe('1d');
    expect(over?.chips.map((chip) => chip.label)).toEqual(['overdue']);
    const waits = view.rows.find((entry) => entry.id === 't2');
    expect(waits?.chips.map((chip) => chip.label)).toEqual(
      expect.arrayContaining(['waiting on a resource', 'waiting 5h']),
    );
  });

  /** A link's label, its href when followable, its tail otherwise. */
  it('shows links with a href only when the target is a URL', () => {
    const links: TaskLinkWire[] = [
      {
        kind: 'pr',
        label: '#1889',
        target: 'https://example.test/pull/1889',
        state: null,
        added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      },
      {
        kind: 'path',
        label: null,
        target: 'docs/plan.md',
        state: null,
        added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      },
    ];
    const wire = wireWith([row(task('t1', 'linked', 'pending', { links }))]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.rows[0]?.links).toEqual([
      { label: '#1889', href: 'https://example.test/pull/1889' },
      { label: 'plan.md', href: null },
    ]);
  });

  /** A project the snapshot does not carry reads as an empty board, named. */
  it('reads a project the snapshot does not carry as an empty board', () => {
    const view = boardView(homeWire, 'TestOrg', 'nope');
    expect(view.empty).toBe(true);
    expect(view.name).toBe('nope');
    expect(view.rows).toEqual([]);
  });

  it('words durations the way the board does', () => {
    expect(fmtSecs(45)).toBe('45s');
    expect(fmtSecs(90)).toBe('1m');
    expect(fmtSecs(7_200)).toBe('2h');
  });
});

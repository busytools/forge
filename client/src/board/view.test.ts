import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus, TaskLinkWire } from '../wire/home';
import { applyMoves, boardView, fmtSecs, landed } from './view';

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
   * Cards file into lanes, one lane per state, in the queue's own order
   * inside each: rank first, then creation, so an unranked row sorts after
   * the ranked ones - the order a lead dispatches from is the order a lane
   * shows. A child carries its epic's subject, and a row waiting on the
   * reader sits in the waiting lane like any other waiting row.
   */
  it('files cards into lanes, rank order inside each', () => {
    const wire = wireWith([
      row(task('late', 'late epic', 'pending', { rank: 5 })),
      row(task('early', 'early epic', 'pending', { rank: 1 })),
      row(task('child', 'a child', 'in_progress', { parent: 'early' })),
      row(
        task('dep', 'a dependency', 'waiting', {
          waiting_on: { kind: 'dependency', detail: null, on: 'other', verification: false },
        }),
      ),
      row(
        task('ask', 'a question', 'waiting', {
          waiting_on: { kind: 'decision', detail: null, on: null, verification: false },
        }),
      ),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    const lane = (key: string) => view.lanes.find((entry) => entry.key === key);
    // Every state has its lane, in the order the states run.
    expect(view.lanes.map((entry) => entry.name)).toEqual([
      'In progress',
      'Waiting',
      'Ready',
      'Completed',
      'Failed',
      'Canceled',
    ]);
    expect(lane('in_progress')?.cards.map((card) => card.id)).toEqual(['child']);
    expect(lane('in_progress')?.cards[0]?.epic).toBe('early epic');
    expect(lane('pending')?.cards.map((card) => card.id)).toEqual(['early', 'late']);
    // A decision wait is a waiting row like any other in the lane; the
    // strip is where the reader acts on it.
    expect(lane('waiting')?.cards.map((card) => card.id)).toEqual(['dep', 'ask']);
    expect(lane('completed')?.cards).toEqual([]);
    expect(view.waiting.map((entry) => entry.id)).toEqual(['ask']);
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
      row(
        task('t3', 'very over', 'in_progress', {
          estimate: { words: '1h', secs: 3_600 },
        }),
        { worked_secs: 9_000 },
      ),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    const cards = view.lanes.flatMap((lane) => lane.cards);
    const card = (id: string) => cards.find((entry) => entry.id === id);
    const over = card('t1');
    expect(over?.worked).toBe('25h');
    expect(over?.estimate).toBe('1d');
    expect(over?.chips.map((chip) => chip.label)).toEqual(['overdue']);
    // Past the estimate the measure takes a tone; past 1.5x it is the loud one.
    expect(over?.ratio).toBe(1);
    expect(over?.tone).toBe('over');
    expect(card('t3')?.tone).toBe('bad');
    expect(card('t3')?.ratio).toBe(1);
    const waits = card('t2');
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
    const card = view.lanes.flatMap((lane) => lane.cards).find((entry) => entry.id === 't1');
    expect(card?.links).toEqual([
      { label: '#1889', href: 'https://example.test/pull/1889' },
      { label: 'plan.md', href: null },
    ]);
  });

  /**
   * A project the snapshot does not carry is UNKNOWN, not empty: the two read
   * the same on a lane count, and only one of them may offer the create line -
   * an unknown project has nothing to file a row against. The name still
   * draws, because the page is a route and the reader addressed it.
   */
  it('reads a project the snapshot does not carry as unknown, not empty', () => {
    const view = boardView(homeWire, 'TestOrg', 'nope');
    expect(view.known, 'an absent project read as one this forge carries').toBe(false);
    expect(view.empty, 'an absent project read as an empty one').toBe(false);
    expect(view.name).toBe('nope');
    expect(view.lanes.every((lane) => lane.cards.length === 0)).toBe(true);
    expect(view.read).toEqual({ onYou: 0, rows: 0, running: 0 });

    // The control: a project the snapshot DOES carry reads as known, so the
    // assertion above is reading the flag rather than a constant.
    expect(boardView(homeWire, 'TestOrg', 'proj').known).toBe(true);
  });

  /**
   * A drag shows at once: applyMoves puts a moved card in its target lane
   * and re-orders inside a lane, without touching the wire.
   */
  it('applies a drag to the lanes before the wire catches up', () => {
    const wire = wireWith([
      row(task('a', 'first', 'pending', { rank: 1 })),
      row(task('b', 'second', 'pending', { rank: 2 })),
    ]);
    const view = boardView(wire, 'TestOrg', 'proj');
    const ids = (lanes: typeof view.lanes, key: string) =>
      lanes.find((lane) => lane.key === key)?.cards.map((card) => card.id);

    // A move to another lane lands at once.
    const movedUp = applyMoves(view.lanes, { b: { to: 'in_progress', before: null, at: 1 } });
    expect(ids(movedUp, 'in_progress')).toEqual(['b']);
    expect(ids(movedUp, 'pending')).toEqual(['a']);

    // A drop above the first card places it there.
    const ranked = applyMoves(view.lanes, { b: { to: 'pending', before: 'a', at: 1 } });
    expect(ids(ranked, 'pending')).toEqual(['b', 'a']);
  });

  /**
   * What the wire has to say before a drag's own placement is dropped: the
   * row's PLACE, not only its state. A re-order inside a lane keeps the
   * status, so a status-only test read every re-order as landed on the next
   * tick and the card snapped back to the order the server still held.
   */
  it('confirms a move by the place the wire put the row in', () => {
    const rows = [
      row(task('a', 'first', 'pending', { rank: 1 })),
      row(task('b', 'second', 'pending', { rank: 2 })),
    ];
    const pending = { to: 'pending' as const, before: 'a', at: 0 };

    // The row is still where it was: the drop is not landed.
    expect(landed(rows, 'b', pending), 'the old order read as landed').toBe(false);
    // A row the wire does not carry at all is not landed either.
    expect(landed(rows, 'gone', pending), 'a row that is not there read as landed').toBe(false);

    // The re-order the wire holds: b sits directly above a, and its status
    // never moved - this is the case a status test cannot see.
    const swapped = [
      row(task('b', 'second', 'pending', { rank: 1 })),
      row(task('a', 'first', 'pending', { rank: 2 })),
    ];
    expect(landed(swapped, 'b', pending), 'the re-order the wire holds').toBe(true);
    // And the end-of-lane drop answers the other way: last is not above a.
    expect(
      landed(swapped, 'b', { to: 'pending', before: null, at: 0 }),
      'a drop at the end read as landed while the row leads the lane',
    ).toBe(false);

    // A cross-lane move is confirmed by the row arriving in that lane.
    const moved = [...rows, row(task('b', 'second', 'in_progress'))];
    expect(landed(moved, 'b', { to: 'in_progress', before: null, at: 0 })).toBe(true);
    expect(landed(rows, 'b', { to: 'in_progress', before: null, at: 0 })).toBe(false);
  });

  it('words durations the way the board does', () => {
    expect(fmtSecs(45)).toBe('45s');
    expect(fmtSecs(90)).toBe('1m');
    expect(fmtSecs(7_200)).toBe('2h');
  });
});

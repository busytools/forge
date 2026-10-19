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
    history: [],
    history_total: 0,
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
    // Every state has its lane, in the order the board draws them: the pool
    // leads, then the lane that wants the reader, then the work.
    expect(view.lanes.map((entry) => entry.name)).toEqual([
      'Backlog',
      'Waiting',
      'In progress',
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
   * **The header's `running` is the In progress lane.** The read took the
   * first lane's count, which is Backlog's: the header said "24 running"
   * while ten rows ran. The state the word names is the one it counts.
   */
  it("counts the header's running from the in-progress lane", () => {
    const wire = wireWith([
      row(task('p1', 'queued', 'pending')),
      row(task('p2', 'queued too', 'pending')),
      row(task('r1', 'running', 'in_progress')),
      row(task('r2', 'also running', 'in_progress')),
      row(task('r3', 'and this one', 'in_progress')),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.read.rows).toBe(5);
    expect(view.read.running, 'the header counted the backlog as running').toBe(3);
  });

  /**
   * **A parent's own tasks, for the row's page**: the children in the
   * queue's order, each labelled with its lane's word, and a child carries
   * its parent's id so the jump has somewhere to land.
   */
  it("lists a parent's own tasks in the queue's order", () => {
    const wire = wireWith([
      row(task('epic', 'the epic', 'pending')),
      row(task('late', 'b child', 'in_progress', { parent: 'epic', rank: 2 })),
      row(task('early', 'a child', 'waiting', { parent: 'epic', rank: 1 })),
      row(task('loose', 'no parent', 'pending')),
    ]);
    const view = boardView(wire, 'TestOrg', 'proj');
    const cards = view.lanes.flatMap((lane) => lane.cards);
    const epic = cards.find((card) => card.id === 'epic');
    expect(epic?.children.map((child) => child.id)).toEqual(['early', 'late']);
    expect(epic?.children[0]?.label).toBe('Waiting \u{b7} a child');
    const child = cards.find((card) => card.id === 'early');
    expect(child?.epicId).toBe('epic');
    expect(child?.epic).toBe('the epic');
    expect(cards.find((card) => card.id === 'loose')?.epicId).toBeNull();
  });

  /**
   * The waiting section carries the rows that wait on the READER - a
   * verification and a question alike - with the facts the action needs.
   */
  it('gathers the rows waiting on the reader', () => {
    const wire = wireWith([
      row(
        task('verify', 'a look', 'waiting', {
          verify: 'user',
          waiting_on: { kind: 'decision', detail: 'the mock', on: null, verification: true },
        }),
        // An epic with two of its four children still open: the strip has to
        // carry that, because the core refuses the approve over it.
        { updated_secs_ago: 2_400, rollup: [2, 4] },
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
      // The epic's four children: two the close still waits on, two it does
      // not. The hold is counted from these rows, not from the rollup.
      row(task('c1', 'first child', 'in_progress', { parent: 'verify' })),
      row(task('c2', 'second child', 'pending', { parent: 'verify' })),
      row(task('c3', 'third child', 'completed', { parent: 'verify' })),
      row(task('c4', 'fourth child', 'canceled', { parent: 'verify' })),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.waiting.map((entry) => entry.id)).toEqual(['verify', 'ask']);
    expect(view.waiting[0]?.verification).toBe(true);
    expect(view.waiting[0]?.age).toBe('40m');
    expect(view.waiting[1]?.verification).toBe(false);
    // The children a root still waits on ride the strip's own row: an approve
    // offered over them is a press the core refuses.
    expect(view.waiting[0]?.open, 'the open children did not reach the strip').toBe(2);
    expect(view.waiting[0]?.rollup).toBe('2/4');
    // A row with no children at all can complete, so nothing is held.
    expect(view.waiting[1]?.open).toBe(0);
    expect(view.waiting[1]?.rollup).toBeNull();
  });

  /**
   * A verification wait belongs to the row's OWN gate (#2423): the strip
   * and the chip show a look only where the row carries `verify: 'user'`,
   * so a park left behind by an inherited epic gate reads as a plain wait
   * rather than claiming the reader owes a look.
   */
  it('keeps a verification park without an own gate out of the reader strip', () => {
    const wire = wireWith([
      row(
        task('own', 'a minted look', 'waiting', {
          verify: 'user',
          waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
        }),
      ),
      row(
        task('inherited', 'an inherited park', 'waiting', {
          waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
        }),
      ),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.waiting.map((entry) => entry.id)).toEqual(['own']);
    const cards = view.lanes.flatMap((lane) => lane.cards);
    const chips = (id: string) => cards.find((card) => card.id === id)?.chips ?? [];
    expect(chips('own').map((chip) => chip.label)).toContain('waiting on you');
    expect(
      chips('inherited').map((chip) => chip.label),
      'an inherited park claimed the reader',
    ).not.toContain('waiting on you');
  });

  /**
   * **What the hold counts, and over what.** The core refuses a root's close
   * only while a child is NOT TERMINAL (`Completed | Failed | Canceled`), and
   * only a root's completion closes a tree at all - so the held set is the
   * root's non-terminal children, not "everything the rollup has not counted
   * as done".
   */
  it('holds a root for the children the core would still refuse over', () => {
    const child = (id: string, status: TaskStatus, parent: string | null) =>
      row(task(id, id, status, { parent }));
    const wire = wireWith([
      row(task('epic', 'the epic', 'pending')),
      child('done', 'completed', 'epic'),
      // A canceled child is terminal: the core accepts the epic's close, and
      // the strip must not draw a hold the core would contradict.
      child('gone', 'canceled', 'epic'),
      // A child with children of its own is still nobody the close counts.
      child('mid', 'waiting', 'epic'),
    ]);

    const view = boardView(wire, 'TestOrg', 'proj');
    expect(view.lanes.flatMap((lane) => lane.cards).length).toBe(4);
    // Nothing is in the strip here; the hold's numbers are what this pins, so
    // they are read off a root put in the gate.
    const gated = wireWith([
      row(
        task('epic2', 'the epic', 'waiting', {
          verify: 'user',
          waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
        }),
        { rollup: [1, 3] },
      ),
      child('done2', 'completed', 'epic2'),
      child('gone2', 'failed', 'epic2'),
      child('open2', 'in_progress', 'epic2'),
    ]);
    const held = boardView(gated, 'TestOrg', 'proj').waiting[0];
    expect(held?.open, 'a failed child was counted as one the close waits on').toBe(1);
    expect(held?.rollup).toBe('1/3');
    expect(held?.root).toBe(true);

    // And a CHILD in the gate is never held: its approval closes no tree.
    const asChild = wireWith([
      row(task('root3', 'the root', 'pending')),
      row(
        task('kid', 'a child', 'waiting', {
          parent: 'root3',
          verify: 'user',
          waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
        }),
        { rollup: [0, 1] },
      ),
      child('grand', 'pending', 'kid'),
    ]);
    const childRow = boardView(asChild, 'TestOrg', 'proj').waiting[0];
    expect(childRow?.root, 'a child row read as a root').toBe(false);
    expect(childRow?.open, 'a child counted children the close never waits on').toBe(1);
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
    expect(over?.time, 'the measure line keeps the space before the slash').toBe('worked 25h / 1d');
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
    const movedUp = applyMoves(view.lanes, {
      b: { to: 'in_progress', before: null, ranked: false, at: 1 },
    });
    expect(ids(movedUp, 'in_progress')).toEqual(['b']);
    expect(ids(movedUp, 'pending')).toEqual(['a']);

    // A drop above the first card places it there.
    const ranked = applyMoves(view.lanes, {
      b: { to: 'pending', before: 'a', ranked: true, at: 1 },
    });
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
    const pending = { to: 'pending' as const, before: 'a', ranked: true, at: 0 };

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
      landed(swapped, 'b', { to: 'pending', before: null, ranked: true, at: 0 }),
      'a drop at the end read as landed while the row leads the lane',
    ).toBe(false);

    // A drag across a lane carries its place too: the move is landed only
    // once the wire holds the row where it was dropped.
    const moved = [...rows, row(task('b', 'second', 'in_progress', { rank: 9 }))];
    expect(
      landed(moved, 'b', { to: 'in_progress', before: 'a', ranked: true, at: 0 }),
      'a move whose place the wire does not hold yet read as landed',
    ).toBe(false);
    const placed = [
      ...rows,
      row(task('b', 'second', 'in_progress', { rank: 1 })),
      row(task('c', 'third', 'in_progress', { rank: 2 })),
    ];
    expect(
      landed(placed, 'b', { to: 'in_progress', before: 'c', ranked: true, at: 0 }),
      'the place the drop asked for',
    ).toBe(true);

    // A keyboard lane step asks for the state alone: it carries no place, so
    // the place must not be demanded - demanding it would read a step that
    // landed as a failure the core never made.
    expect(
      landed(moved, 'b', { to: 'in_progress', before: 'a', ranked: false, at: 0 }),
      'a lane step the wire landed read as a failure because its rank moved',
    ).toBe(true);
    // A row the wire carries in some OTHER status is not landed, even when
    // the drop asked for the state alone.
    expect(
      landed(rows, 'b', { to: 'in_progress', before: null, ranked: false, at: 0 }),
      'a row still in its old status read as landed',
    ).toBe(false);
  });

  it('words durations the way the board does', () => {
    expect(fmtSecs(45)).toBe('45s');
    expect(fmtSecs(90)).toBe('1m');
    expect(fmtSecs(7_200)).toBe('2h');
  });
});

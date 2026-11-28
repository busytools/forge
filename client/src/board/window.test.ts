/**
 * The window's own decisions: what needs the reader, what looks wrong, who is
 * doing what, and what is next - each derived from the server's facts and
 * nothing re-computed.
 */

import { describe, expect, it } from 'vitest';

import type { BoardWire } from '../wire/board';
import type { BoardRow, HomeWire, Task } from '../wire/home';
import type { WindowView } from './window';
import { clockOf, windowView } from './window';

const ORG = 'TestOrg';
const PROJECT = 'proj';

function task(id: string, status: Task['status'], over: Partial<Task> = {}): Task {
  return {
    id,
    project_name: PROJECT,
    subject: `subject ${id}`,
    active_form: null,
    detail: null,
    status,
    kind: 'finite',
    owner: null,
    parent: null,
    waiting_on: null,
    request: null,
    estimate: null,
    cadence: null,
    rank: null,
    verify: null,
    links: [],
    evidence: null,
    version: 0,
    archived_at: null,
    created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    updated_at: { secs_since_epoch: 1_000, nanos_since_epoch: 0 },
    ...over,
  };
}

function row(task: Task, over: Partial<BoardRow> = {}): BoardRow {
  return {
    task,
    worked_secs: 0,
    updated_secs_ago: 0,
    owner_live: true,
    wait_secs: null,
    marks: { ready: false, in_review: false, overdue: false, stale: false, to_close: false },
    rollup: null,
    parent_subject: null,
    history: [],
    history_total: 0,
    ...over,
  };
}

function wires(rows: BoardRow[], agents: unknown[] = [], fleet: unknown[] = []) {
  return {
    home: { agents, fleet } as unknown as HomeWire,
    board: {
      projects: [
        {
          project: { org: ORG, name: PROJECT },
          rows,
          crons: [],
          connectors: { gotify: [], slack: [] },
        },
      ],
    } as BoardWire,
  };
}

const NOW = 10_000;

function viewOf(
  rows: BoardRow[],
  agents: unknown[] = [],
  fleet: unknown[] = [],
  now = NOW,
): WindowView {
  const held = wires(rows, agents, fleet);
  return windowView(held.home, held.board, ORG, PROJECT, now);
}

describe('the window', () => {
  it('carries a decision ask with its options, its evidence and its seat', () => {
    const ask = task('t1', 'waiting', {
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: false,
        since: { secs_since_epoch: NOW - 120, nanos_since_epoch: 0 },
      },
      request: {
        kind: 'decision',
        question: 'Ship it behind a flag?',
        options: ['flag it', 'ship it'],
        answer: null,
      },
      evidence: {
        commit: 'abc123',
        path: null,
        lead_verified: null,
        excerpt: '12 tests run: 12 passed',
        verified_at: null,
        problem: null,
      },
      owner: { org: ORG, project: PROJECT, label: 'w-1' },
    });
    const asked = viewOf([row(ask)]);
    expect(asked.needs).toHaveLength(1);
    const card = asked.needs[0];
    expect(card?.kind).toBe('decision');
    expect(card?.question).toBe('Ship it behind a flag?');
    expect(card?.options).toEqual(['flag it', 'ship it']);
    expect(card?.evidence).toBe('12 tests run: 12 passed');
    expect(card?.evidenceState).toBe('unchecked');
    expect(card?.owner).toBe('w-1');
    expect(card?.seat?.label).toBe('w-1');
    expect(card?.age).toBe('2m');
  });

  it("carries the lead's verification as the evidence line when the row has one", () => {
    const ask = task('t1', 'waiting', {
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: false,
        since: { secs_since_epoch: NOW - 60, nanos_since_epoch: 0 },
      },
      request: {
        kind: 'decision',
        question: 'Ship it?',
        options: ['ship it', 'wait'],
        answer: null,
      },
      evidence: {
        commit: null,
        path: null,
        lead_verified: 'read the sibling repo; it passes',
        excerpt: 'the excerpt is not the line',
        verified_at: { secs_since_epoch: NOW - 30, nanos_since_epoch: 0 },
        problem: null,
      },
    });
    const card = viewOf([row(ask)]).needs[0];
    expect(card?.evidence).toBe('lead-verified: read the sibling repo; it passes');
    expect(card?.evidenceState).toBe('verified');
  });

  it('ranks a look ahead of a decision, then the oldest wait first', () => {
    const look = task('t1', 'waiting', {
      verify: 'user',
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: true,
        since: { secs_since_epoch: NOW - 10, nanos_since_epoch: 0 },
      },
    });
    const older = task('t2', 'waiting', {
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: false,
        since: { secs_since_epoch: NOW - 500, nanos_since_epoch: 0 },
      },
      request: { question: 'which shape?', options: [], answer: null },
    });
    const newer = task('t3', 'waiting', {
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: false,
        since: { secs_since_epoch: NOW - 60, nanos_since_epoch: 0 },
      },
      request: { question: 'now or later?', options: [], answer: null },
    });
    const asked = viewOf([row(look), row(older), row(newer)]);
    expect(asked.needs.map((card) => card.id)).toEqual(['t1', 't2', 't3']);
    expect(asked.needs[0]?.kind).toBe('look');
    expect(asked.needs[0]?.question).toContain('Approve');
  });

  it('never draws a wait on the user as an exception', () => {
    const ask = task('t1', 'waiting', {
      waiting_on: {
        kind: 'user',
        detail: null,
        on: null,
        verification: false,
        since: { secs_since_epoch: 0, nanos_since_epoch: 0 },
      },
    });
    const asked = viewOf([
      row(ask, {
        marks: { ready: false, in_review: false, overdue: true, stale: true, to_close: false },
      }),
    ]);
    expect(asked.exceptions).toEqual([]);
  });

  it('states a contradiction, a dead owner and an untouchcd row as timestamps', () => {
    const refused = task('t1', 'pending', {
      evidence: {
        commit: 'deadbeef',
        path: null,
        lead_verified: null,
        excerpt: null,
        verified_at: null,
        problem: 'commit deadbeef does not exist',
      },
    });
    const dead = task('t2', 'in_progress', {
      owner: { org: ORG, project: PROJECT, label: 'gone' },
      updated_at: { secs_since_epoch: 3_000, nanos_since_epoch: 0 },
    });
    const still = task('t3', 'in_progress', {
      owner: { org: ORG, project: PROJECT, label: 'w-1' },
      updated_at: { secs_since_epoch: 3_600, nanos_since_epoch: 0 },
    });
    const asked = viewOf([
      row(refused),
      row(dead, { owner_live: false }),
      row(still, {
        marks: { ready: false, in_review: false, overdue: false, stale: true, to_close: false },
      }),
    ]);
    const lines = asked.exceptions.map((item) => item.line);
    expect(lines[0]).toContain('forge could not confirm its evidence: commit deadbeef');
    expect(lines[0], 'the evidence line carries its own timestamp').toContain('no change since');
    expect(lines[1]).toContain('has no session behind it');
    expect(lines[2]).toBe(`no change since ${clockOf(3_600)}`);
  });

  it('times a stopped worker by its last activity, and omits the time when none is known', () => {
    const seat = { org: ORG, project: PROJECT, label: 'w-2' };
    const miss = [
      {
        project: PROJECT,
        misses: [{ kind: 'no-row' as const, label: 'w-2 holds no row', worker: 'w-2' }],
      },
    ];
    const seen = viewOf(
      [],
      [
        {
          slot: seat,
          label: 'w-2',
          lifecycle: 'Idle',
          last_activity: { secs_since_epoch: 3_600, nanos_since_epoch: 0 },
        },
      ],
      miss,
    );
    expect(seen.exceptions[0]?.line).toBe(
      `holds no row since ${clockOf(3_600)} - feed it or despawn it`,
    );
    const quiet = viewOf(
      [],
      [{ slot: seat, label: 'w-2', lifecycle: 'Idle', last_activity: null }],
      miss,
    );
    expect(quiet.exceptions[0]?.line).toBe('holds no row - feed it or despawn it');
  });

  it('carries a stopped worker from the fleet as an exception', () => {
    // The fleet's misses arrive narrowed by `missFrom` (wire/home.ts): the
    // window reads the client's own union, never a raw server shape.
    const asked = viewOf(
      [row(task('t1', 'in_progress', { owner: { org: ORG, project: PROJECT, label: 'w-1' } }))],
      [],
      [
        {
          project: PROJECT,
          misses: [{ kind: 'no-row', label: 'w-2 holds no row', worker: 'w-2' }],
        },
      ],
    );
    expect(asked.exceptions).toEqual([
      { id: null, subject: 'w-2', line: 'holds no row - feed it or despawn it', owner: 'w-2' },
    ]);
    expect(asked.counts.stopped).toBe(1);
  });

  it('draws one line per seat that HOLDS work, as provenance', () => {
    const seat = { org: ORG, project: PROJECT, label: 'builder' };
    const held = task('t1', 'in_progress', {
      owner: seat,
      active_form: 'Folding the board',
      estimate: { words: '2h', secs: 7_200 },
      links: [
        {
          kind: 'pr',
          label: 'PR #9',
          target: 'https://example.invalid/pull/9',
          state: null,
          added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        },
      ],
    });
    const idle = { org: ORG, project: PROJECT, label: 'watcher' };
    const second = task('t2', 'in_progress', {
      owner: seat,
      active_form: 'Sweeping the archive',
    });
    const asked = viewOf(
      [row(held), row(second)],
      [
        {
          slot: seat,
          label: 'builder',
          lifecycle: 'Running',
          last_activity: { secs_since_epoch: 5_000, nanos_since_epoch: 0 },
        },
        { slot: idle, label: 'watcher', lifecycle: 'Idle', last_activity: null },
      ],
    );
    const builder = asked.live.find((line) => line.label === 'builder');
    expect(builder?.doing).toBe('Folding the board');
    expect(builder?.since).toBe(5_000);
    expect(builder?.artifact).toBe('PR #9');
    expect(builder?.next).toBe('2h');
    expect(builder?.held, 'one line per seat, counting the rest').toBe(2);
    // A seat holding nothing is the fleet count's and the stopped-worker
    // exception's business, not the live list's.
    expect(asked.live.find((line) => line.label === 'watcher')).toBeUndefined();
  });

  it("lists the ready queue in the lead's dispatch order", () => {
    const queued = (id: string, rank: number, kind: Task['kind']) =>
      row(
        task(id, 'pending', {
          rank,
          kind,
          estimate: kind === 'finite' ? { words: '1d', secs: 86_400 } : null,
          cadence: kind === 'standing' ? { words: '1w', secs: 604_800 } : null,
        }),
        { marks: { ready: true, in_review: false, overdue: false, stale: false, to_close: false } },
      );
    const asked = viewOf([queued('late', 9, 'finite'), queued('first', 1, 'standing')]);
    expect(asked.next.map((item) => item.id)).toEqual(['first', 'late']);
    expect(asked.next[0]?.standing).toBe(true);
    expect(asked.next[0]?.measure).toBe('1w');
    expect(asked.next[1]?.measure).toBe('1d');
    expect(asked.counts.ready).toBe(2);
  });

  it('carries each ask kind, and only a decision carries options', () => {
    const asked = (kind: 'clarification' | 'exception' | 'teaching') =>
      row(
        task(`t-${kind}`, 'waiting', {
          waiting_on: {
            kind: 'user',
            detail: null,
            on: null,
            verification: false,
            since: { secs_since_epoch: NOW - 60, nanos_since_epoch: 0 },
          },
          request: { kind, question: 'what next?', options: ['stale'], answer: null },
        }),
      );
    const view = viewOf([asked('clarification'), asked('exception'), asked('teaching')]);
    expect(view.needs.map((card) => card.kind)).toEqual(['clarification', 'exception', 'teaching']);
    // A decision's options are its value shape; the other kinds carry none.
    for (const card of view.needs) expect(card.options, card.kind).toEqual([]);
  });

  it('reads a snooze until a time against the clock, and until-moves against the row', () => {
    const at = row(
      task('t1', 'waiting', {
        updated_at: { secs_since_epoch: 500, nanos_since_epoch: 0 },
        waiting_on: {
          kind: 'user',
          detail: null,
          on: null,
          verification: false,
          since: { secs_since_epoch: 100, nanos_since_epoch: 0 },
          snoozed: {
            until: { at: { secs_since_epoch: NOW + 600, nanos_since_epoch: 0 } },
            since: { secs_since_epoch: 500, nanos_since_epoch: 0 },
          },
        },
      }),
    );
    expect(viewOf([at]).needs[0]?.snoozed, 'a timed snooze still holds').toBe(true);
    expect(viewOf([at], [], [], NOW + 601).needs[0]?.snoozed, 'the time passed').toBe(false);

    const moved = row(
      task('t2', 'waiting', {
        updated_at: { secs_since_epoch: 700, nanos_since_epoch: 0 },
        waiting_on: {
          kind: 'user',
          detail: null,
          on: null,
          verification: false,
          since: { secs_since_epoch: 100, nanos_since_epoch: 0 },
          snoozed: { until: 'activity', since: { secs_since_epoch: 500, nanos_since_epoch: 0 } },
        },
      }),
    );
    expect(viewOf([moved]).needs[0]?.snoozed, 'the row moved under the snooze').toBe(false);
  });

  it('reads the deadline and the name of the default that runs on it', () => {
    const due = row(
      task('t1', 'waiting', {
        waiting_on: {
          kind: 'user',
          detail: null,
          on: null,
          verification: false,
          since: { secs_since_epoch: 0, nanos_since_epoch: 0 },
          acknowledged_at: { secs_since_epoch: 10, nanos_since_epoch: 0 },
          due: {
            at: { secs_since_epoch: NOW + 3_600, nanos_since_epoch: 0 },
            default: 'escalate',
            ran_at: null,
          },
        },
      }),
    );
    const card = viewOf([due]).needs[0];
    expect(card?.acknowledged, 'an addressed ask says so').toBe(true);
    expect(card?.dueAt).toBe(NOW + 3_600);
    expect(card?.dueDefault).toBe('escalate');
  });

  it('reads the attention facts, the delta and the sample off the board read', () => {
    const held = wires([]);
    const project = held.board.projects[0];
    if (project === undefined) throw new Error('the fixture holds no project');
    held.board = {
      projects: [
        {
          ...project,
          attention: {
            last_look: { secs_since_epoch: NOW - 3_600, nanos_since_epoch: 0 },
            checked_at: { secs_since_epoch: NOW, nanos_since_epoch: 0 },
            agents: 2,
            delta: { moved: 3, landed: 1, expired: 0, merged: 2 },
            sample: {
              rows: [
                {
                  seat: 'w-1',
                  tool: 'Bash',
                  detail: 'cargo nextest run',
                  at: { secs_since_epoch: NOW - 60, nanos_since_epoch: 0 },
                },
              ],
              sampled: 1,
              total: 7,
            },
          },
        },
      ],
    };
    const view = windowView(held.home, held.board, ORG, PROJECT, NOW);
    expect(view.attention?.lastLook).toBe(NOW - 3_600);
    expect(view.attention?.merged).toBe(2);
    expect(view.attention?.sample.rows[0]?.detail).toBe('cargo nextest run');
    expect(view.attention?.sample.sampled).toBe(1);
    expect(view.attention?.sample.total).toBe(7);
    // A payload older than the field says so rather than guessing.
    expect(viewOf([]).attention).toBeNull();
  });

  it('keeps unknown a project the snapshot does not carry, and empty a quiet one', () => {
    const absent = windowView(wires([]).home, { projects: [] }, ORG, PROJECT, NOW);
    expect(absent.state).toBe('unknown');
    const quiet = viewOf([]);
    expect(quiet.state).toBe('empty');
    expect(quiet.needs).toEqual([]);
    expect(quiet.exceptions).toEqual([]);
    expect(quiet.next).toEqual([]);
  });
});

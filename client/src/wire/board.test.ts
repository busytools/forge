import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';

import { repoPath } from '../repo';
import {
  BOARD_VARIANTS,
  applyDetail,
  boardFrom,
  carryDetail,
  coversBoard,
  type BoardWire,
} from './board';
import type { SessionSlot } from './types';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A Rust variant name as the wire spells it. */
function snake(name: string): string {
  return name.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();
}

/**
 * The variants the server's `Subject::Board::covers` hears, read out of its
 * own source: the arm itself, plus the two helper lists it calls.
 *
 * **The client's list is a second copy**, and nothing else ties it to the
 * server's - `coversHome` has its own source read in `fleet.test.ts`, and a
 * variant that reaches the board on one side and not the other is a mark
 * that stops moving silently.
 */
function serverBoardVariants(): { names: string[]; arm: number; seat: number } {
  const source = readFileSync(repoPath('crates/forge-server/src/transport/envelope.rs'), 'utf8');
  const armStart = source.indexOf('Self::Board =>');
  const armEnd = source.indexOf('Self::Session(', armStart);
  if (armStart < 0 || armEnd < 0) {
    throw new Error('envelope.rs no longer holds the `Self::Board` cover arm');
  }
  const helper = (name: string): string => {
    const start = source.indexOf(`fn ${name}(`);
    if (start < 0) throw new Error(`envelope.rs no longer holds \`${name}\``);
    return source.slice(start, source.indexOf('\n}\n', start));
  };
  const collect = (body: string): string[] =>
    [...body.matchAll(/SessionUpdate::([A-Za-z0-9]+)/g)]
      .map((match) => match[1])
      .filter((name): name is string => name !== undefined)
      .map(snake);

  const inArm = collect(source.slice(armStart, armEnd));
  const seat = collect(helper('seat_liveness'));
  const detail = collect(helper('board_detail'));
  return {
    names: [...new Set([...inArm, ...seat, ...detail])],
    arm: inArm.length,
    seat: seat.length,
  };
}

/**
 * The board parse is the one place a task's newer fields enter, and a field
 * it drops draws nothing: no mark, no reserved chip, no parked wait. These
 * pin the parse rather than the renderer, so a regression here fails by
 * name instead of quietly losing the board's words (#2423).
 */
function parsedTask(task: Record<string, unknown>): Record<string, unknown> {
  const raw = {
    projects: [{ work: { gate: 'in_repo' }, rows: [{ task }] }],
  } as unknown as BoardWire;
  const taskRow = boardFrom(raw).projects[0]?.rows[0]?.task;
  if (taskRow === undefined) throw new Error('the row did not parse');
  return taskRow as unknown as Record<string, unknown>;
}

describe('the board hears exactly the variants that move it (#2517)', () => {
  it('takes its own three and the seat events its rows read', () => {
    expect(coversBoard({ tasks_changed: { key: LEAD, tasks: [] } })).toBe(true);
    expect(coversBoard({ cron_schedules_changed: { key: LEAD, crons: [] } })).toBe(true);
    expect(
      coversBoard({ connector_subscriptions_changed: { key: LEAD, gotify: [], slack: [] } }),
    ).toBe(true);
    // A row's owner-gone chip is read from seat liveness, so these reach it.
    for (const variant of [
      'spawning',
      'connected',
      'session_replaced',
      'releasing',
      'connection_failed',
      'worker_status_changed',
    ]) {
      expect(coversBoard({ [variant]: { key: LEAD } }), `${variant} was dropped`).toBe(true);
    }
    expect(coversBoard({ accounts_changed: {} })).toBe(false);
    expect(coversBoard('catalog_loaded')).toBe(false);
    // A turn frame moves no board row's marks.
    expect(coversBoard({ turn_complete: { key: LEAD } })).toBe(false);
  });

  /**
   * The mirror, read off the server's own source rather than this file's
   * idea of it: a variant the server sends to the board and this list does
   * not know is a mark that stops moving, silently.
   */
  it('knows every variant the server sends the board, read from envelope.rs', () => {
    const { names, arm, seat } = serverBoardVariants();
    // Controls: an extractor that read nothing would pass the loop below.
    expect(arm, 'the Board arm was not read out of envelope.rs').toBeGreaterThanOrEqual(1);
    expect(seat, 'seat_liveness was not read out of envelope.rs').toBeGreaterThanOrEqual(6);
    expect(names.length, 'the Board cover set was not read at all').toBeGreaterThanOrEqual(9);

    for (const name of names) {
      expect(
        coversBoard({ [name]: { key: LEAD } }),
        `${name} reaches the board on the server`,
      ).toBe(true);
    }
    for (const name of BOARD_VARIANTS) {
      expect(names, `${name} is the client's alone`).toContain(name);
    }
  });
});

describe('the task fields the board marks', () => {
  it('keeps the attestation, the reservation and a parked wait', () => {
    const task = parsedTask({
      id: 't-1',
      project_name: 'proj',
      subject: 'a look',
      status: 'completed',
      attested: {
        evidence: 'Looks right - merge',
        at: { secs_since_epoch: 1, nanos_since_epoch: 0 },
      },
      reserved: 'user',
      waiting_on: { kind: 'user', detail: null, on: null, verification: false, parked: true },
    });
    const attested = task['attested'] as { evidence?: string } | null;
    expect(attested?.evidence, 'the quoted approval reached the row').toBe('Looks right - merge');
    expect(task['reserved'], 'the reserved pick reached the row').toBe('user');
    const wait = task['waiting_on'] as { parked?: boolean } | null;
    expect(wait?.parked, 'the parked flag reached the wait').toBe(true);
  });

  it('defaults absent newer fields rather than dropping the row', () => {
    // A payload that states none of the newer fields: no attestation, no
    // reservation, and a wait that is not parked.
    const task = parsedTask({
      id: 't-2',
      project_name: 'proj',
      subject: 'an older row',
      status: 'waiting',
      waiting_on: { kind: 'user', detail: null, on: null, verification: false },
    });
    expect(task['attested']).toBeNull();
    expect(task['reserved']).toBeNull();
    const wait = task['waiting_on'] as { parked?: boolean } | null;
    expect(wait?.parked, 'an older server sent no parked flag').toBe(false);
  });
});

/**
 * The drawer's own read: the bulk board carries the window's fields only, so
 * a `task_detail` frame lands here - one row whole, with its brief and its
 * history - and replaces what the bulk read said about it.
 */
describe("the drawer's row", () => {
  function row(over: Record<string, unknown>, history: unknown[] = []): Record<string, unknown> {
    return {
      task: {
        id: 't-1',
        project_name: 'proj',
        subject: 'the row',
        status: 'in_progress',
        links: [],
        created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
        ...over,
      },
      worked_secs: 0,
      updated_secs_ago: 0,
      owner_live: true,
      marks: { ready: false, in_review: false, overdue: false, stale: false, to_close: false },
      rollup: null,
      parent_subject: null,
      history,
      history_total: history.length,
    };
  }

  const wire = (): BoardWire =>
    boardFrom({
      projects: [
        {
          project: { org: 'TestOrg', name: 'proj' },
          rows: [row({})],
          crons: [],
          connectors: { gotify: [], slack: [] },
        },
      ],
    } as unknown as BoardWire);

  it('replaces the bulk row with the whole record', () => {
    const step = {
      task_id: 't-1',
      project_name: 'proj',
      from: 'pending',
      to: 'in_progress',
      at: { secs_since_epoch: 5, nanos_since_epoch: 0 },
      by: 'system',
    };
    const applied = applyDetail(
      wire(),
      { org: 'TestOrg', name: 'proj' },
      row({ detail: 'the brief the drawer reads' }, [step]),
    );
    const held = applied?.projects[0]?.rows[0];
    expect(held?.task.detail).toBe('the brief the drawer reads');
    expect(held?.history, 'the history rode the frame').toHaveLength(1);
    expect(applied?.projects[0]?.rows, 'the row was replaced, not duplicated').toHaveLength(1);
  });

  it('keeps a closed row out of the lanes, in the drawer set, and a null answer changes nothing', () => {
    const closed = applyDetail(
      wire(),
      { org: 'TestOrg', name: 'proj' },
      row({ id: 't-9', subject: 'an ended row', status: 'completed' }),
    );
    expect(closed?.projects[0]?.rows, 'a closed row stays out of the lanes').toHaveLength(1);
    // The shelf's subject asks for the closed row whole; the page's drawer
    // draws it from this set, since the bulk read never carries it.
    expect(closed?.projects[0]?.detail?.map((entry) => entry.task.id)).toEqual(['t-9']);
    expect(applyDetail(wire(), { org: 'TestOrg', name: 'proj' }, null)).toBeNull();
  });

  it('carries the drawer set across a whole read, and drops a row the read now has', () => {
    const first = applyDetail(
      wire(),
      { org: 'TestOrg', name: 'proj' },
      row({ id: 't-9', subject: 'an ended row', status: 'completed' }),
    );
    const reread = wire();
    const carried = carryDetail(first, reread);
    expect(
      carried.projects[0]?.detail?.map((entry) => entry.task.id),
      "a whole read never carries the drawer's rows, and must not drop them",
    ).toEqual(['t-9']);
    // The same row back in the lanes' read is the fresher copy: the detail
    // copy steps aside.
    const liveAgain = boardFrom({
      projects: [
        {
          project: { org: 'TestOrg', name: 'proj' },
          rows: [row({ id: 't-9', status: 'in_progress' })],
          crons: [],
          connectors: { gotify: [], slack: [] },
        },
      ],
    } as unknown as BoardWire);
    expect(carryDetail(first, liveAgain).projects[0]?.detail).toEqual([]);
  });

  it('updates a drawer row in place rather than growing the set', () => {
    const first = applyDetail(
      wire(),
      { org: 'TestOrg', name: 'proj' },
      row({ id: 't-9', subject: 'an ended row', status: 'completed', detail: 'first' }),
    );
    const next = applyDetail(
      first,
      { org: 'TestOrg', name: 'proj' },
      row({ id: 't-9', subject: 'an ended row', status: 'completed', detail: 'second' }),
    );
    expect(next?.projects[0]?.detail, 'one entry per row').toHaveLength(1);
    expect(next?.projects[0]?.detail?.[0]?.task.detail).toBe('second');
  });
});

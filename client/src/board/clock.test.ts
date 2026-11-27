import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import type { BoardWire } from '../wire/board';
import type { BoardRow } from '../wire/home';
import { STALE_SECS, agedBoard, agedRow } from './clock';

/**
 * The core's window, read out of its source.
 *
 * **A mirror that is checked is not a drift risk.** The client derives the
 * no-movement marks at draw time and needs the same window the server used;
 * reading the constant here means a changed window reddens this build rather
 * than leaving a chip that stops crossing.
 */
function serverStaleSecs(): number {
  const source = readFileSync(
    fileURLToPath(new URL('../../../crates/forge-workspace/src/board.rs', import.meta.url)),
    'utf8',
  );
  const found = /^pub const DEFAULT_STALE_SECS: u64 = (\d+) \* 3600;$/m.exec(source);
  if (found === null) {
    throw new Error(
      'board.rs no longer declares DEFAULT_STALE_SECS as `N * 3600`, so this pin reads nothing',
    );
  }
  return Number(found[1]) * 3600;
}

/** One row at `updatedAt`, in `status`, as the server encodes it. */
function row(over: Partial<BoardRow> = {}, task: Record<string, unknown> = {}): BoardRow {
  return {
    task: {
      id: 't1',
      project_name: 'proj',
      status: 'in_progress',
      updated_at: { secs_since_epoch: 1_000, nanos_since_epoch: 0 },
      estimate: null,
      waiting_on: null,
      ...task,
    },
    worked_secs: 60,
    updated_secs_ago: 0,
    marks: {
      ready: false,
      in_review: false,
      overdue: false,
      no_movement: false,
      waiting_too_long: false,
      stale: false,
      to_close: false,
    },
    rollup: null,
    parent_subject: null,
    history: [],
    history_total: 0,
    ...over,
  } as unknown as BoardRow;
}

describe('the board clock', () => {
  it('mirrors the core no-movement window', () => {
    expect(STALE_SECS).toBe(serverStaleSecs());
  });

  /** **The server's worked figure already counts the open interval**, so the
   * page only adds what has elapsed since the record was encoded. */
  it('advances a running row by the time since the record landed', () => {
    const encodedAt = 1_000_000;
    const at = (seconds: number) => agedRow(row(), encodedAt / 1000 + seconds, encodedAt);
    expect(at(0).worked_secs).toBe(60);
    expect(at(90).worked_secs).toBe(150);

    // A row that is not running does not accrue time.
    const done = row({}, { status: 'completed' });
    expect(agedRow(done, encodedAt / 1000 + 90, encodedAt).worked_secs).toBe(60);
  });

  /** Ages come off the server's own pair, so they are exact however long the
   * page has held the record. */
  it('derives the age from the task rather than the encode', () => {
    const encodedAt = 1_700_000_000_000;
    const encodedSecs = Math.floor(encodedAt / 1000);
    const now = encodedSecs + 500;
    const fresh = row({}, { updated_at: { secs_since_epoch: encodedSecs, nanos_since_epoch: 0 } });
    expect(agedRow(fresh, now, encodedAt).updated_secs_ago).toBe(500);
  });

  /** **A page whose clock is far from the server's draws the same age.** The
   * record's `updated_secs_ago` was measured on the server's clock, and only
   * the time since the read is the page's to add - comparing the client's now
   * to the record's absolute stamp would be wrong by the skew. */
  it('advances the age without trusting the client clock', () => {
    const encodedAt = 1_700_000_000_000;
    const aged = row(
      { updated_secs_ago: 300 },
      { updated_at: { secs_since_epoch: 1_000, nanos_since_epoch: 0 } },
    );
    expect(agedRow(aged, Math.floor(encodedAt / 1000) + 60, encodedAt).updated_secs_ago).toBe(360);
  });

  /** `overdue` crosses from the advanced worked time; the two window marks
   * cross from the age, and a parked wait carries neither. */
  it('crosses the time marks at the server thresholds', () => {
    const encodedAt = 1_700_000_000_000;
    const nowSecs = Math.floor(encodedAt / 1000);
    const estimate = { words: '1h', secs: 100 };

    const before = agedRow(row({}, { estimate }), nowSecs + 30, encodedAt);
    expect(before.worked_secs).toBe(90);
    expect(before.marks.overdue, 'overdue crossed before the estimate').toBe(false);

    const after = agedRow(row({}, { estimate }), nowSecs + 60, encodedAt);
    expect(after.worked_secs).toBe(120);
    expect(after.marks.overdue, 'overdue did not cross at the estimate').toBe(true);

    const old = row(
      {},
      { status: 'waiting', updated_at: { secs_since_epoch: nowSecs, nanos_since_epoch: 0 } },
    );
    const stale = agedRow(old, nowSecs + STALE_SECS + 1, encodedAt);
    expect(stale.marks.no_movement).toBe(true);
    expect(stale.marks.waiting_too_long, 'a waiting row past the window marks too').toBe(true);

    const parked = row(
      {},
      {
        status: 'waiting',
        updated_at: { secs_since_epoch: nowSecs, nanos_since_epoch: 0 },
        waiting_on: { kind: 'decision', detail: null, on: null, verification: false, parked: true },
      },
    );
    const deferred = agedRow(parked, nowSecs + STALE_SECS + 1, encodedAt);
    expect(deferred.marks.no_movement, 'a parked wait carries no movement mark').toBe(false);
    expect(deferred.marks.waiting_too_long, 'a parked wait carries no wait mark').toBe(false);
  });

  /** The marks that are not about time are the server's and pass through. */
  it('keeps the server marks that time does not move', () => {
    const encodedAt = 1_700_000_000_000;
    const marked = row({
      marks: {
        ready: true,
        in_review: true,
        overdue: false,
        no_movement: false,
        waiting_too_long: false,
        stale: true,
        to_close: true,
      },
    });
    const aged = agedRow(marked, Math.floor(encodedAt / 1000) + 5, encodedAt);
    expect(aged.marks.ready).toBe(true);
    expect(aged.marks.in_review).toBe(true);
    expect(aged.marks.stale).toBe(true);
    expect(aged.marks.to_close).toBe(true);
  });

  it('advances every project and row of a record', () => {
    const encodedAt = 1_700_000_000_000;
    const board = {
      projects: [
        { project: { org: 'TestOrg', name: 'proj' }, rows: [row()], crons: [], connectors: {} },
      ],
    } as unknown as BoardWire;
    const aged = agedBoard(board, encodedAt + 90_000, encodedAt);
    expect(aged.projects[0]?.rows[0]?.worked_secs).toBe(150);
  });
});

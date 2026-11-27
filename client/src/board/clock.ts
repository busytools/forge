/**
 * The board's time-derived figures, moved by the page's own clock.
 *
 * **A subscription sends nothing for a subject that did not change**, so the
 * server no longer re-encodes the board on a timer to advance figures that
 * only the clock moves. The record still carries the server's own derivation
 * as of the instant it was encoded; this walks the drawn rows forward from
 * that instant to now, using the row's own timestamps and the same rules the
 * core applies (`crates/forge-workspace/src/board.rs`):
 *
 * - `worked_secs` grows by the elapsed time while the row is in progress - the
 *   server's figure already counts the open interval up to its encode;
 * - `updated_secs_ago` is now less the task's own `updated_at`;
 * - `overdue`, `no_movement` and `waiting_too_long` cross at the same
 *   thresholds the core uses.
 *
 * The marks that are not about time - ready, in review, owner gone, to close -
 * stay exactly as the server derived them, and a field added to a row later
 * passes through untouched.
 */

import type { BoardWire } from '../wire/board';
import type { BoardRow } from '../wire/home';

/**
 * The no-movement window, in seconds.
 *
 * A mirror of the core's `DEFAULT_STALE_SECS`, and
 * `board/clock.test.ts` reads the constant out of the Rust source so the two
 * cannot drift: a changed window there is a red build here, not a chip that
 * stops crossing.
 */
export const STALE_SECS = 4 * 3600;

/** One row advanced to `nowSecs`, from the instant the server encoded it. */
export function agedRow(row: BoardRow, nowSecs: number, readAtMs: number): BoardRow {
  // **A row a frame introduced is anchored to that frame**, not to the whole
  // read before it: its figure is empty as of the frame, and counting from
  // the read would credit it with work done before it existed.
  const encodedAt = Math.floor((row.read_at ?? readAtMs) / 1000);
  const elapsed = Math.max(0, nowSecs - encodedAt);
  const inProgress = row.task.status === 'in_progress';
  const worked = row.worked_secs + (inProgress ? elapsed : 0);
  const updatedAgo = Math.max(0, nowSecs - row.task.updated_at.secs_since_epoch);
  const parked = row.task.waiting_on?.parked === true;
  const estimate = row.task.estimate;
  return {
    ...row,
    worked_secs: worked,
    updated_secs_ago: updatedAgo,
    marks: {
      ...row.marks,
      overdue: estimate !== null && estimate.secs > 0 && worked > estimate.secs,
      no_movement: !parked && updatedAgo > STALE_SECS,
      waiting_too_long: row.task.status === 'waiting' && !parked && updatedAgo > STALE_SECS,
    },
  };
}

/** Every project's rows advanced to `nowSecs`. */
export function agedBoard(board: BoardWire, nowMs: number, readAtMs: number): BoardWire {
  const nowSecs = Math.floor(nowMs / 1000);
  return {
    ...board,
    projects: board.projects.map((project) => ({
      ...project,
      rows: project.rows.map((row) => agedRow(row, nowSecs, readAtMs)),
    })),
  };
}

/**
 * The board's time-derived figures, moved by the page's own clock.
 *
 * **A subscription sends nothing for a subject that did not change**, so the
 * server no longer re-encodes the board on a timer to advance figures that
 * only the clock moves. The record still carries the server's own derivation
 * as of the instant it was encoded; this walks the drawn rows forward from
 * that instant to now with the server's own figures plus the client's elapsed
 * time, under the same rules the core applies
 * (`crates/forge-workspace/src/board.rs`):
 *
 * - `worked_secs` grows by the elapsed time while the row is in progress - the
 *   server's figure already counts the open interval up to its encode - and a
 *   row a frame introduced is anchored to that frame, not to the read before
 *   it;
 * - `updated_secs_ago` is the server's own age plus the elapsed time, so a
 *   page whose clock is far from the server's still draws the right age;
 * - `overdue` and `stale` cross at the same thresholds the core uses: a
 *   finite row past its estimate or a wait past the band is overdue, and a
 *   row nobody has moved (or whose owner is gone) is stale.
 *
 * The marks that are not about time - ready, in review, owner gone's own
 * slot, to close - stay exactly as the server derived them, and a field added
 * to a row later passes through untouched.
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
  // **The age advances from the server's own pair, not from the client's
  // clock against the record's absolute stamp.** `updated_secs_ago` and
  // `updated_at` were computed together at the encode, so the age is that
  // figure plus the time elapsed since the read - a page whose clock is
  // minutes off still draws the right age, where comparing two absolute
  // instants from two clocks would be wrong by the skew.
  const updatedAgo = row.updated_secs_ago + elapsed;
  const parked = row.task.waiting_on?.parked === true;
  const estimate = row.task.estimate;
  return {
    ...row,
    worked_secs: worked,
    updated_secs_ago: updatedAgo,
    marks: {
      ...row.marks,
      overdue:
        (row.task.kind === 'finite' &&
          estimate !== null &&
          estimate.secs > 0 &&
          worked > estimate.secs) ||
        (row.task.status === 'waiting' &&
          !parked &&
          (row.wait_secs ?? updatedAgo) > STALE_SECS),
      stale: !parked && (updatedAgo > STALE_SECS || !row.owner_live),
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

/**
 * The board's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it: every project's rows, schedules and connector sets (#2517).
 *
 * **Its own subject, not the home's.** These three fields were most of a
 * ~708 KB home read, and a home read runs on every update that covers the
 * fleet; a page that draws none of them should not pay for them. The page and
 * the rail that do draw them subscribe here.
 */

import type { SessionUpdate } from '../protocol';
import { rowFrom, type BoardRow, type CronEntry } from './home';

/**
 * One project's identity, which is all a board row needs to place it. The
 * rest of the home's project view - its sessions, accounts and paths - is not
 * carried here: the board neither hears nor draws it (#2517).
 */
export interface ProjectBoardIdentity {
  org: string;
  name: string;
}

/** One project's board detail. */
export interface ProjectBoardWire {
  project: ProjectBoardIdentity;
  /** The project's live rows, with the board's own facts on each. */
  rows: BoardRow[];
  crons: CronEntry[];
  /**
   * This project's connector subscriptions, one list per connector.
   *
   * Left `unknown` here like the home's own member and narrowed where it
   * enters: the sets are per project, so they ride the board rather than the
   * home's `connectors`, which carries the liveness facts alone.
   */
  connectors: unknown;
}

export interface BoardWire {
  projects: ProjectBoardWire[];
}

/** What the board store holds: the read, or the server's own refusal. */
export interface BoardRead {
  wire: BoardWire | null;
  refused: string | null;
}

/**
 * Narrow the snapshot where it enters, so every union downstream is already
 * one of the values the board draws.
 */
export function boardFrom(data: BoardWire): BoardWire {
  return {
    projects: data.projects.map((row) => ({
      ...row,
      rows: (row.rows ?? []).map(rowFrom),
      crons: row.crons ?? [],
    })),
  };
}

/**
 * Whether a board subscriber hears this update - the server's own
 * `Subject::Board::covers`, and the one place it lives on this side.
 *
 * The three variants that move what the board's snapshot carries - a task
 * write, a schedule set and a connector set - and the six seat events a row's
 * `stale` mark is read from (a seat starting, connecting, being replaced,
 * releasing, failing to connect, or a worker's status moving). A row's ages
 * move with the clock instead, which the page's own beat answers. The home
 * hears the task write too, because its fleet counts are derived from the
 * same tasks; it hears neither of the other sets, because its payload no
 * longer carries them.
 */
const BOARD_VARIANTS: readonly string[] = [
  'tasks_changed',
  'cron_schedules_changed',
  'connector_subscriptions_changed',
  // A row's owner-gone chip is read from seat liveness.
  'spawning',
  'connected',
  'session_replaced',
  'releasing',
  'connection_failed',
  'worker_status_changed',
];

export function coversBoard(update: SessionUpdate): boolean {
  if (typeof update === 'string') return false;
  const [variant] = Object.keys(update);
  return variant !== undefined && BOARD_VARIANTS.includes(variant);
}

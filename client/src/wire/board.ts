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
  /**
   * When the drawn record was received, ms since the epoch: the instant the
   * server's worked time and ages were computed.
   *
   * **The page's clock moves the record's time-derived figures from here**
   * (`board/clock.ts`); absent on a fixture, where the record is drawn as of
   * now and nothing has elapsed.
   */
  readAt?: number;
  /**
   * The stream skipped a version, so one whole read is on its way.
   *
   * The record above is still drawn - a gap never blanks a page - and this is
   * what says the figures may be a beat behind.
   */
  resyncing?: boolean;
}

/**
 * Narrow the snapshot where it enters, so every union downstream is already
 * one of the values the board draws.
 */
export function boardFrom(data: BoardWire): BoardWire {
  return {
    // **A record missing its list is drawn empty rather than crashing the
    // page reading it.** A malformed answer is a state to draw; a throw
    // inside a store notification would take the page down with it.
    projects: (Array.isArray(data.projects) ? data.projects : []).map((row) => ({
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
export const BOARD_VARIANTS: readonly string[] = [
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

/** One frame's project key, as the board places it: org and project name. */
function projectOf(key: unknown): { org: string; name: string } | null {
  if (typeof key !== 'object' || key === null) return null;
  const org = (key as { org?: unknown }).org;
  const name = (key as { project?: unknown }).project;
  if (typeof org !== 'string' || typeof name !== 'string') return null;
  return { org, name };
}

/**
 * The project's task set a `tasks_changed` frame carries, or `null`.
 *
 * **The frame carries the whole set**, so a task write is applied to the drawn
 * rows rather than answered with a whole board read (rule 32; the read was
 * 3 MB and ran at 0.39/s while a seat was on screen - #3063).
 */
export function tasksFrame(
  update: SessionUpdate,
): { project: { org: string; name: string }; tasks: unknown[] } | null {
  if (typeof update === 'string') return null;
  const payload = (update as { tasks_changed?: { key?: unknown; tasks?: unknown } }).tasks_changed;
  if (payload === undefined || !Array.isArray(payload.tasks)) return null;
  const project = projectOf(payload.key);
  if (project === null) return null;
  return { project, tasks: payload.tasks };
}

/** The project's schedule set a `cron_schedules_changed` frame carries, or `null`. */
export function cronsFrame(
  update: SessionUpdate,
): { project: { org: string; name: string }; crons: unknown[] } | null {
  if (typeof update === 'string') return null;
  const payload = (update as { cron_schedules_changed?: { key?: unknown; crons?: unknown } })
    .cron_schedules_changed;
  if (payload === undefined || !Array.isArray(payload.crons)) return null;
  const project = projectOf(payload.key);
  if (project === null) return null;
  return { project, crons: payload.crons };
}

/**
 * The project's connector sets a `connector_subscriptions_changed` frame
 * carries, or `null`.
 *
 * **The frame carries both sets whole**, exactly as the snapshot's
 * `ProjectConnectorsWire` holds them, so a board that already holds the
 * project draws them from the frame. A frame missing either set is one this
 * build cannot read: the caller keeps its read, because the failure direction
 * is a read rather than a wrong value on screen.
 */
export function connectorsFrame(
  update: SessionUpdate,
): { project: { org: string; name: string }; gotify: unknown[]; slack: unknown[] } | null {
  if (typeof update === 'string') return null;
  const payload = (
    update as {
      connector_subscriptions_changed?: { key?: unknown; gotify?: unknown; slack?: unknown };
    }
  ).connector_subscriptions_changed;
  if (payload === undefined) return null;
  if (!Array.isArray(payload.gotify) || !Array.isArray(payload.slack)) return null;
  const project = projectOf(payload.key);
  if (project === null) return null;
  return { project, gotify: payload.gotify, slack: payload.slack };
}

/**
 * Apply one project's task set to the drawn board, without a read.
 *
 * **The task facts come from the frame; the board's derived facts stay what
 * the last read said.** `worked_secs`, the marks, the rollup, the parent's
 * subject and the transition history are the server's own computation and no
 * frame carries them - so a row that was drawn keeps them, a row that is new
 * draws with their empty values, and the marks beat refreshes them (at most
 * five minutes, the staleness the beat already accepts). A row the frame does
 * not name is left alone: the core's set is the project's live rows, and a
 * removal arrives in the same frame as an absence - the beat reconciles the
 * rest rather than a client guessing that an omission is a delete.
 *
 * Returns `null` when the board holds no snapshot to apply into (the first
 * read is still owed), which is the one case the caller still reads.
 */
export function applyTasks(
  wire: BoardWire | null,
  project: { org: string; name: string },
  tasks: readonly unknown[],
): BoardWire | null {
  if (wire === null) return null;
  const at = wire.projects.findIndex(
    (held) => held.project.org === project.org && held.project.name === project.name,
  );
  // A project the drawn board does not hold has nowhere to apply into: the
  // caller still owes a read (#3074 review).
  if (at === -1) return null;
  const held = wire.projects[at];
  if (held === undefined) return null;
  const byId = new Map(held.rows.map((row) => [row.task.id, row] as const));
  const order: string[] = [];
  for (const entry of tasks) {
    const fresh = rowFrom({ task: entry });
    const prior = byId.get(fresh.task.id);
    byId.set(
      fresh.task.id,
      prior === undefined
        ? fresh
        : {
            ...prior,
            task: fresh.task,
          },
    );
    order.push(fresh.task.id);
  }
  // **The frame is the whole live set**, so a row it does not name is gone:
  // the drawn order stays for the rows it does name (the core's own
  // `tasks_for_project` and the board's rows are the same population), and a
  // row the frame introduces is appended.
  const named = new Set(order);
  const rows: BoardRow[] = [];
  for (const row of held.rows) {
    if (named.has(row.task.id)) {
      const next = byId.get(row.task.id);
      if (next !== undefined) rows.push(next);
    }
  }
  for (const id of order) {
    if (!held.rows.some((row) => row.task.id === id)) {
      const added = byId.get(id);
      if (added !== undefined) rows.push(added);
    }
  }
  const projects = [...wire.projects];
  projects[at] = { ...held, rows };
  return { projects };
}

/** Apply one project's schedule set to the drawn board, without a read. */
export function applyCrons(
  wire: BoardWire | null,
  project: { org: string; name: string },
  crons: readonly unknown[],
): BoardWire | null {
  if (wire === null) return null;
  const at = wire.projects.findIndex(
    (held) => held.project.org === project.org && held.project.name === project.name,
  );
  if (at === -1) return null;
  const held = wire.projects[at];
  if (held === undefined) return null;
  const projects = [...wire.projects];
  projects[at] = { ...held, crons: crons as CronEntry[] };
  return { projects };
}

/**
 * Apply one project's connector sets to the drawn board, without a read.
 *
 * **Both sets come from the frame whole**, and only into a project the drawn
 * board holds: a project it does not hold has nowhere to apply into, so the
 * caller keeps its read. The sets are left `unknown` here and narrowed by
 * `rowConnectors` at the draw, exactly as the snapshot's own copy is.
 */
export function applyConnectors(
  wire: BoardWire | null,
  project: { org: string; name: string },
  gotify: readonly unknown[],
  slack: readonly unknown[],
): BoardWire | null {
  if (wire === null) return null;
  const at = wire.projects.findIndex(
    (held) => held.project.org === project.org && held.project.name === project.name,
  );
  if (at === -1) return null;
  const held = wire.projects[at];
  if (held === undefined) return null;
  const projects = [...wire.projects];
  projects[at] = { ...held, connectors: { gotify, slack } };
  return { projects };
}

/**
 * One project's board, gathered from the snapshot and nothing else.
 *
 * The board is a view of the same wire the fleet reads: its rows carry the
 * server's own marks, and this module only decides what a reader sees -
 * the rank order, the two levels, the chips' words. Nothing here
 * re-derives a fact.
 */

import { followable } from '../home/view';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus } from '../wire/home';

/** One row on the board, resolved for the markup. */
export interface BoardRowView {
  id: string;
  /** 0 is an epic (or a standalone row); 1 is a child under the one above. */
  depth: 0 | 1;
  display: string;
  subject: string;
  status: TaskStatus;
  owner: string | null;
  /** Worked time against the estimate, in the board's words. */
  worked: string;
  estimate: string | null;
  /** What the row's marks say, in words: overdue, no movement, on you... */
  chips: { label: string; tone: 'bad' | 'warn' | 'blue' | 'dim' }[];
  /** The references it carries, as short labels with a href when followable. */
  links: { label: string; href: string | null }[];
  /** A parent's children: `done/total`. */
  rollup: string | null;
}

/** One row waiting on the user, as the board's section draws it. */
export interface WaitingView {
  id: string;
  subject: string;
  owner: string | null;
  /** The verify gate, or a question the worker raised. */
  verification: boolean;
  detail: string | null;
  /** How long it has waited. */
  age: string;
}

export interface BoardView {
  name: string;
  org: string;
  rows: BoardRowView[];
  waiting: WaitingView[];
  /** Whether the project holds no live rows at all. */
  empty: boolean;
}

/** A duration as the board's words: `3h`, `12m`, `45s`. */
export function fmtSecs(secs: number): string {
  if (secs >= 3_600) return `${Math.floor(secs / 3_600)}h`;
  if (secs >= 60) return `${Math.floor(secs / 60)}m`;
  return `${secs}s`;
}

/** The chips a row's marks and its wait imply, in reading order. */
function chipsOf(task: Task, marks: Marks, updatedSecsAgo: number): BoardRowView['chips'] {
  const chips: BoardRowView['chips'] = [];
  if (task.waiting_on !== null) {
    const wait = task.waiting_on;
    chips.push(
      wait.verification
        ? { label: 'waiting on you', tone: 'warn' }
        : wait.kind === 'decision'
          ? { label: 'waiting on you', tone: 'warn' }
          : wait.kind === 'dependency'
            ? { label: `on ${wait.on ?? 'another row'}`, tone: 'warn' }
            : { label: 'waiting on a resource', tone: 'warn' },
    );
    if (marks.waiting_too_long)
      chips.push({ label: `waiting ${fmtSecs(updatedSecsAgo)}`, tone: 'bad' });
  }
  if (marks.overdue) chips.push({ label: 'overdue', tone: 'bad' });
  if (marks.no_movement && task.waiting_on === null) {
    chips.push({ label: `no movement ${fmtSecs(updatedSecsAgo)}`, tone: 'dim' });
  }
  if (marks.stale) chips.push({ label: 'owner gone', tone: 'bad' });
  if (marks.in_review) chips.push({ label: 'in review', tone: 'blue' });
  if (marks.to_close) chips.push({ label: 'to close', tone: 'blue' });
  if (task.attempt > 1) chips.push({ label: `attempt ${task.attempt}`, tone: 'dim' });
  return chips;
}

/** The links a row shows: short labels, with a href when the target is a URL. */
function linksOf(task: Task): BoardRowView['links'] {
  return task.links.map((link) => ({
    label: link.label ?? shortTarget(link.target),
    href: followable(link.target),
  }));
}

/** A bare target as a short label: its last path segment. */
function shortTarget(target: string): string {
  const trimmed = target.replace(/\/+$/, '');
  const parts = trimmed.split('/');
  return parts[parts.length - 1] ?? trimmed;
}

/** One board row, resolved. */
function rowView(row: BoardRow, depth: 0 | 1): BoardRowView {
  const task = row.task;
  return {
    id: task.id,
    depth,
    display:
      task.status === 'in_progress' && task.active_form !== null && task.active_form !== ''
        ? task.active_form
        : task.subject,
    subject: task.subject,
    status: task.status,
    owner: task.owner === null ? null : task.owner.label,
    worked: fmtSecs(row.worked_secs),
    estimate: task.estimate === null ? null : task.estimate.words,
    chips: chipsOf(task, row.marks, row.updated_secs_ago),
    links: linksOf(task),
    rollup: row.rollup === null ? null : `${row.rollup[0]}/${row.rollup[1]}`,
  };
}

/**
 * `(org, project)`'s board: the ranked rows, two levels (an epic, then its
 * children), and the rows waiting on the user. A project no snapshot
 * carries reads as an empty board with its name - the page is a route, and
 * a route can be addressed before the first frame lands.
 */
export function boardView(wire: HomeWire, org: string, project: string): BoardView {
  const row = wire.projects.find(
    (entry) => entry.project.org === org && entry.project.name === project,
  );
  if (row === undefined) {
    return { name: project, org, rows: [], waiting: [], empty: true };
  }
  // The rank order the server would dispatch from: rank first, then
  // creation, so an unranked row sorts after the ranked ones.
  const ordered = [...row.rows].sort((a, b) => {
    const rankA = a.task.rank ?? Number.MAX_SAFE_INTEGER;
    const rankB = b.task.rank ?? Number.MAX_SAFE_INTEGER;
    if (rankA !== rankB) return rankA - rankB;
    return a.task.created_at.secs_since_epoch - b.task.created_at.secs_since_epoch;
  });
  const epics = ordered.filter((entry) => entry.task.parent === null);
  const rows: BoardRowView[] = [];
  for (const epic of epics) {
    rows.push(rowView(epic, 0));
    for (const child of ordered.filter((entry) => entry.task.parent === epic.task.id)) {
      rows.push(rowView(child, 1));
    }
  }
  const waiting = ordered
    .filter(
      (entry) => entry.task.status === 'waiting' && entry.task.waiting_on?.kind === 'decision',
    )
    .map((entry) => ({
      id: entry.task.id,
      subject: entry.task.subject,
      owner: entry.task.owner === null ? null : entry.task.owner.label,
      verification: entry.task.waiting_on?.verification === true,
      detail: entry.task.waiting_on?.detail ?? null,
      age: fmtSecs(entry.updated_secs_ago),
    }));
  return { name: project, org, rows, waiting, empty: row.rows.length === 0 };
}

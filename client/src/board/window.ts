/**
 * The window: the board drawn for a person, exceptions first.
 *
 * The research the retrofit rests on says a reader of a fleet does not want
 * one card per agent - they want what needs them, what looks wrong, who is
 * doing what, and what is next. So this module derives exactly those four
 * regions from the server's own facts, and it derives nothing the server
 * already decided: the marks, the wait, the evidence's verdict and the agent
 * rows arrive computed, and this file only orders and words them.
 *
 * **Staleness is a timestamp, never an adjective.** A row that has not moved
 * reads `no change since 14:02`, because "stale" asks the reader to do the
 * arithmetic and motion is not progress either.
 */

import { followable } from '../home/view';
import type { BoardWire } from '../wire/board';
import type { BoardRow, HomeWire, Task } from '../wire/home';

/** One card under NEEDS YOU: a decision, or a look. */
export interface NeedCard {
  id: string;
  kind: 'decision' | 'look';
  subject: string;
  /** The ask itself: the decision's question, or the look's own sentence. */
  question: string;
  /** The choices the lead enumerated, empty for a look. */
  options: string[];
  /** The bounded excerpt behind the ask, when the row carries one. */
  evidence: string | null;
  /** What the gate said about that evidence; `null` for a row with none. */
  evidenceState: 'verified' | 'unchecked' | 'failed' | null;
  /** The artifact the ask points at, when it carries a followable one. */
  artifact: { label: string; href: string } | null;
  owner: string | null;
  /** When the wait began, in epoch seconds - the card's age is read from it. */
  at: number;
  age: string;
  /** The owner's seat, for the discuss door, when it is a live seat. */
  seat: { org: string; project: string; label: string } | null;
}

/** One row under EXCEPTIONS: a fact that contradicts the state, not a badge. */
export interface ExceptionCard {
  /** The row, or `null` for a fleet-level exception (a stopped worker). */
  id: string | null;
  subject: string;
  /** What is wrong, in one line, with the evidence behind it. */
  line: string;
  owner: string | null;
}

/** One line under LIVE: a seat and what it is actually doing. */
export interface LiveLine {
  label: string;
  seat: { org: string; project: string; label: string };
  /** The row it holds, in the row's own running words; null when it holds none. */
  doing: string | null;
  /** When the seat was last active, in epoch seconds; null is unknown. */
  since: number | null;
  /** The artifact the held row last touched, by name. */
  artifact: string | null;
  /** The row's estimate or cadence, the next thing it is measured by. */
  next: string | null;
}

/** One row of the ranked ready queue. */
export interface NextRow {
  id: string;
  subject: string;
  owner: string | null;
  /** The estimate (finite) or cadence (standing), as the row words it. */
  measure: string | null;
  standing: boolean;
}

export interface WindowView {
  project: string;
  /** `unknown` is a project this snapshot does not carry - never `empty`. */
  state: 'unknown' | 'empty' | 'known';
  needs: NeedCard[];
  exceptions: ExceptionCard[];
  live: LiveLine[];
  next: NextRow[];
  counts: {
    /** Rows the server marks in progress. */
    inProgress: number;
    /** Ready and unowned: the queue's own count. */
    ready: number;
    /** Rows waiting on the reader. */
    onYou: number;
    /** Live workers holding no row. */
    stopped: number;
  };
}

/** A duration as the board's words: `3h`, `12m`, `45s`. */
export function fmtSecs(secs: number): string {
  if (secs >= 86_400) return `${Math.floor(secs / 86_400)}d`;
  if (secs >= 3_600) return `${Math.floor(secs / 3_600)}h`;
  if (secs >= 60) return `${Math.floor(secs / 60)}m`;
  return `${secs}s`;
}

/** An instant as a wall clock: `14:02`, the reader's own timezone. */
export function clockOf(secs: number): string {
  const at = new Date(secs * 1_000);
  return `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
}

/** A bare target as a short label: its last path segment. */
function shortTarget(target: string): string {
  const trimmed = target.replace(/\/+$/, '');
  const parts = trimmed.split('/');
  return parts[parts.length - 1] ?? trimmed;
}

/** The first followable link a row carries, as a door. */
function artifactOf(task: Task): { label: string; href: string } | null {
  for (const link of task.links) {
    const href = followable(link.target);
    if (href !== null) return { label: link.label ?? shortTarget(link.target), href };
  }
  return null;
}

/** The evidence's own state, in the server's three words. */
function evidenceStateOf(task: Task): NeedCard['evidenceState'] {
  const evidence = task.evidence;
  if (evidence === null || evidence === undefined) return null;
  if (evidence.verified_at !== null) return 'verified';
  if (evidence.problem !== null) return 'failed';
  return 'unchecked';
}

/** Whether a wait is the reader's: a decision, or a look park carrying its own gate. */
function onTheUser(row: BoardRow): boolean {
  const wait = row.task.waiting_on;
  if (wait === null) return false;
  return wait.verification ? row.task.verify === 'user' : wait.kind === 'decision';
}

/** The instant a row's wait began: the wait's own stamp, else the row's touch. */
function waitAt(task: Task): number {
  return task.waiting_on?.since?.secs_since_epoch ?? task.updated_at.secs_since_epoch;
}

function needCardOf(row: BoardRow, nowSecs: number): NeedCard | null {
  const task = row.task;
  if (task.status !== 'waiting' || !onTheUser(row)) return null;
  const wait = task.waiting_on;
  const verification = wait?.verification === true;
  const at = waitAt(task);
  const decision = task.decision ?? null;
  const question = verification
    ? `Approve ${task.subject} or say what to change`
    : (decision?.question ?? wait?.detail?.trim() ?? `A decision is waiting on ${task.subject}`);
  return {
    id: task.id,
    kind: verification ? 'look' : 'decision',
    subject: task.subject,
    question,
    options: verification ? [] : (decision?.options ?? []),
    evidence: task.evidence?.excerpt ?? null,
    evidenceState: evidenceStateOf(task),
    artifact: artifactOf(task),
    owner: task.owner === null ? null : task.owner.label,
    at,
    age: fmtSecs(Math.max(0, nowSecs - at)),
    seat: task.owner === null ? null : { ...task.owner },
  };
}

/**
 * One exception, or none: a row whose evidence the gate refused, a row
 * nobody has touched past the band, a row whose owner is gone - and a worker
 * holding nothing, which the fleet's own misses name.
 */
function exceptionOf(row: BoardRow): ExceptionCard | null {
  const task = row.task;
  const owner = task.owner === null ? null : task.owner.label;
  const evidence = task.evidence;
  if (evidence !== null && evidence !== undefined && evidence.problem !== null) {
    return {
      id: task.id,
      subject: task.subject,
      line: `the gate refused its evidence: ${evidence.problem}`,
      owner,
    };
  }
  if (!row.owner_live && task.owner !== null) {
    return {
      id: task.id,
      subject: task.subject,
      line: `${owner} has no session behind it - no change since ${clockOf(
        task.updated_at.secs_since_epoch,
      )}`,
      owner,
    };
  }
  if (row.marks.stale && task.waiting_on === null) {
    return {
      id: task.id,
      subject: task.subject,
      line: `no change since ${clockOf(task.updated_at.secs_since_epoch)}`,
      owner,
    };
  }
  return null;
}

/** The rank order the lead would dispatch from: rank, then creation. */
function ordered(rows: BoardRow[]): BoardRow[] {
  return [...rows].sort((a, b) => {
    const rankA = a.task.rank ?? Number.MAX_SAFE_INTEGER;
    const rankB = b.task.rank ?? Number.MAX_SAFE_INTEGER;
    if (rankA !== rankB) return rankA - rankB;
    return a.task.created_at.secs_since_epoch - b.task.created_at.secs_since_epoch;
  });
}

/**
 * `(org, project)`'s window. A project the board's snapshot does not carry is
 * UNKNOWN rather than empty: the page is a route, and a route can be
 * addressed before the first frame lands.
 */
export function windowView(
  wire: HomeWire,
  board: BoardWire,
  org: string,
  project: string,
  nowSecs: number,
): WindowView {
  const held = board.projects.find(
    (entry) => entry.project.org === org && entry.project.name === project,
  );
  const base: WindowView = {
    project,
    state: 'unknown',
    needs: [],
    exceptions: [],
    live: [],
    next: [],
    counts: { inProgress: 0, ready: 0, onYou: 0, stopped: 0 },
  };
  if (held === undefined) return base;
  const rows = held.rows;

  // NEEDS YOU: the asks on the reader, ranked oldest first, looks ahead of
  // decisions because a look is a finished row nobody can close until the
  // reader looks. The rank is the wait's own age, and nothing else.
  const needs = rows
    .map((row) => needCardOf(row, nowSecs))
    .filter((card): card is NeedCard => card !== null)
    .sort((a, b) => (a.kind === b.kind ? a.at - b.at : a.kind === 'look' ? -1 : 1));

  // EXCEPTIONS: what contradicts the state. A stopped worker is the fleet's
  // own miss, drawn here because this is the page where the work is.
  const fleet = wire.fleet.find((entry) => entry.project === project);
  const exceptions: ExceptionCard[] = rows
    .map((row) => exceptionOf(row))
    .filter((card): card is ExceptionCard => card !== null);
  for (const miss of fleet?.misses ?? []) {
    const stopped = stoppedWorkerOf(miss);
    if (stopped !== null) {
      exceptions.push({
        id: null,
        subject: stopped,
        line: 'holds no row - feed it or despawn it',
        owner: stopped,
      });
    }
  }

  // LIVE: one line per seat this project has, saying what it holds. The
  // agent rows are the home's; the row is the board's. A seat between turns
  // holds its row and is not doing anything yet - drawn as unknown, never as
  // progress.
  const byOwner = new Map<string, BoardRow>();
  for (const row of rows) {
    const label = row.task.owner?.label;
    if (label !== undefined && row.task.status === 'in_progress' && !byOwner.has(label)) {
      byOwner.set(label, row);
    }
  }
  const live: LiveLine[] = wire.agents
    .filter((agent) => agent.slot.org === org && agent.slot.project === project)
    .map((agent) => {
      const busy = byOwner.get(agent.label);
      const last = agent.last_activity?.secs_since_epoch ?? null;
      const link = busy === undefined ? null : artifactOf(busy.task);
      const measure =
        busy === undefined ? null : (busy.task.estimate?.words ?? busy.task.cadence?.words ?? null);
      return {
        label: agent.label,
        seat: { ...agent.slot },
        doing: busy === undefined ? null : (busy.task.active_form ?? busy.task.subject),
        since: last ?? (busy === undefined ? null : busy.task.updated_at.secs_since_epoch),
        artifact: link === null ? null : link.label,
        next: measure,
      };
    })
    .sort((a, b) => a.label.localeCompare(b.label));

  // NEXT: the ranked ready queue the lead dispatches from.
  const next: NextRow[] = ordered(rows)
    .filter((row) => row.marks.ready && row.task.owner === null)
    .map((row) => ({
      id: row.task.id,
      subject: row.task.subject,
      owner: null,
      measure: row.task.estimate?.words ?? row.task.cadence?.words ?? null,
      standing: row.task.kind === 'standing',
    }));

  return {
    project,
    state: rows.length === 0 && live.length === 0 ? 'empty' : 'known',
    needs,
    exceptions,
    live,
    next,
    counts: {
      inProgress: rows.filter((row) => row.task.status === 'in_progress').length,
      ready: next.length,
      onYou: needs.length,
      stopped: exceptions.filter((card) => card.id === null).length,
    },
  };
}

/** The worker's label a fleet miss names, where the miss is a stopped one. */
function stoppedWorkerOf(miss: unknown): string | null {
  if (typeof miss !== 'object' || miss === null) return null;
  const held = (miss as { unaccounted_worker?: unknown }).unaccounted_worker;
  return typeof held === 'string' ? held : null;
}

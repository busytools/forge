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

import { artifactOf, waitOnUser } from './view';
import type { BoardWire } from '../wire/board';
import type { BoardRow, HomeWire, MissRow, Task } from '../wire/home';

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
  /**
   * Why the approve door is held, when it is: a root row with open
   * children cannot close on a look. `null` when approve is offered.
   */
  approveBlocked: string | null;
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
  /** How many rows the seat holds in progress: one line, all of them. */
  held: number;
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

/** The evidence's own state, in the server's three words. */
function evidenceStateOf(task: Task): NeedCard['evidenceState'] {
  const evidence = task.evidence;
  if (evidence === null || evidence === undefined) return null;
  if (evidence.verified_at !== null) return 'verified';
  if (evidence.problem !== null) return 'failed';
  return 'unchecked';
}

/** The instant a row's wait began: the wait's own stamp, else the row's touch. */
function waitAt(task: Task): number {
  return task.waiting_on?.since?.secs_since_epoch ?? task.updated_at.secs_since_epoch;
}

function needCardOf(row: BoardRow, nowSecs: number): NeedCard | null {
  const task = row.task;
  if (task.status !== 'waiting' || !waitOnUser(task)) return null;
  const wait = task.waiting_on;
  const verification = wait?.verification === true;
  const at = waitAt(task);
  const decision = task.decision ?? null;
  const artifact = artifactOf(task);
  // The ask says what to do, and a look names the thing to look at: the
  // artifact's own label when the row carries one, else the row.
  const looked = artifact === null ? task.subject : artifact.label;
  const question = verification
    ? `Approve ${looked} or say what to change`
    : (decision?.question ?? wait?.detail?.trim() ?? `A decision is waiting on ${task.subject}`);
  // Two holds the core would refuse the press for, drawn instead of a door:
  // a root closes its whole tree on completion and cannot while any child
  // is open, and a completion or approve rests on evidence the gate has
  // verified.
  const evidenceState = evidenceStateOf(task);
  const openChildren = row.rollup !== null && row.rollup[0] < row.rollup[1];
  const approveBlocked = !verification
    ? null
    : task.parent === null && row.rollup !== null && openChildren
      ? `This row closes its epic: ${row.rollup[1] - row.rollup[0]} child ${
          row.rollup[1] - row.rollup[0] === 1 ? 'row is' : 'rows are'
        } still open. Close ${row.rollup[1] - row.rollup[0] === 1 ? 'it' : 'them'} first.`
      : evidenceState !== 'verified'
        ? 'This row has no verified evidence behind its completion; move it with an evidence claim first.'
        : null;
  return {
    id: task.id,
    kind: verification ? 'look' : 'decision',
    subject: task.subject,
    question,
    options: verification ? [] : (decision?.options ?? []),
    evidence: task.evidence?.excerpt ?? null,
    evidenceState,
    artifact,
    owner: task.owner === null ? null : task.owner.label,
    at,
    // The server's own wait age: the wait's start, never the row's last edit.
    age: fmtSecs(row.wait_secs ?? Math.max(0, nowSecs - at)),
    approveBlocked,
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
      line: `forge could not confirm its evidence: ${
        evidence.problem
      } - no change since ${clockOf(task.updated_at.secs_since_epoch)}`,
      owner,
    };
  }
  // **Backlog is the queue's, not an exception**: a ready unowned row is
  // what the lead dispatches from, and drawing every aged one here reads
  // the queue as a fault.
  if (row.marks.ready && task.owner === null) return null;
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

  // EXCEPTIONS: what contradicts the state - and never a row already drawn
  // as an ask, which would report the same fact twice. A stopped worker is
  // the fleet's own miss, drawn here because this is the page where the
  // work is.
  const fleet = wire.fleet.find((entry) => entry.project === project);
  const asked = new Set(needs.map((card) => card.id));
  const exceptions: ExceptionCard[] = rows
    .filter((row) => !asked.has(row.task.id))
    .map((row) => exceptionOf(row))
    .filter((card): card is ExceptionCard => card !== null);
  for (const miss of fleet?.misses ?? []) {
    const stopped = stoppedWorkerOf(miss);
    if (stopped !== null) {
      // The line is a timestamp like the row exceptions': when the seat was
      // last active is the fact behind "nothing for it to do".
      const last = wire.agents.find(
        (agent) =>
          agent.slot.org === org && agent.slot.project === project && agent.label === stopped,
      )?.last_activity;
      const since = last === null || last === undefined ? null : clockOf(last.secs_since_epoch);
      exceptions.push({
        id: null,
        subject: stopped,
        line:
          since === null
            ? 'holds no row - feed it or despawn it'
            : `holds no row since ${since} - feed it or despawn it`,
        owner: stopped,
      });
    }
  }

  // LIVE: one line per seat HOLDING work on this project, saying what it
  // holds. A seat holding nothing is the fleet's business (the header
  // count and the stopped-worker exception), not this list's. The agent
  // rows are the home's; the row is the board's. A seat between turns
  // holds its row and is not doing anything yet - drawn as unknown, never
  // as progress.
  const byOwner = new Map<string, BoardRow[]>();
  for (const row of rows) {
    const label = row.task.owner?.label;
    if (label !== undefined && row.task.status === 'in_progress') {
      byOwner.set(label, [...(byOwner.get(label) ?? []), row]);
    }
  }
  const live: LiveLine[] = wire.agents
    .filter((agent) => agent.slot.org === org && agent.slot.project === project)
    .filter((agent) => (byOwner.get(agent.label) ?? []).length > 0)
    .map((agent) => {
      const held = byOwner.get(agent.label) ?? [];
      // The line names one row and counts the rest: a seat holding three
      // rows is one seat, and a line per row would draw the same seat three
      // times.
      const busy = held[0];
      const last = agent.last_activity?.secs_since_epoch ?? null;
      const link = busy === undefined ? undefined : artifactOf(busy.task);
      const measure =
        busy === undefined ? null : (busy.task.estimate?.words ?? busy.task.cadence?.words ?? null);
      return {
        label: agent.label,
        seat: { ...agent.slot },
        doing: busy === undefined ? null : (busy.task.active_form ?? busy.task.subject),
        // The seat's own last activity, never the row's touch: a seat that
        // has not acted yet is unknown, and the line says so.
        since: last,
        artifact: link === undefined || link === null ? null : link.label,
        next: measure,
        held: held.length,
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

/**
 * The worker's label a fleet miss names, where the miss is a stopped one.
 * The miss arrives narrowed by `missFrom` (wire/home.ts), so this reads
 * the client's own union - never a raw server shape.
 */
function stoppedWorkerOf(miss: MissRow): string | null {
  return miss.kind === 'no-row' ? miss.worker : null;
}

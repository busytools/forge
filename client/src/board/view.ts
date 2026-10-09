/**
 * One project's board, gathered from the snapshot and nothing else.
 *
 * The board is a view of the same wire the fleet reads: its cards carry
 * the server's own marks, and this module only decides what a reader sees
 * - the lanes, the order inside them, the words on a card. Nothing here
 * re-derives a fact.
 */

import { followable } from '../home/view';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus } from '../wire/home';

/** One card on the board, resolved for the markup. */
export interface BoardCardView {
  id: string;
  subject: string;
  /** What the first line says: the active form while it runs, else the subject. */
  display: string;
  /** A row's parent subject, for a child card's meta line. */
  epic: string | null;
  status: TaskStatus;
  owner: string | null;
  /** The owner picker's initial: `lead` is one person here, so one letter. */
  initials: string;
  /** Worked time against the estimate, in the board's words. */
  worked: string;
  estimate: string | null;
  /** How far worked runs against the estimate: 0-1 for the rule under it. */
  ratio: number;
  /** Past the estimate, or past 1.5x of it - the rule takes the tone. */
  tone: 'over' | 'bad' | null;
  /** What the row's marks say, in words: overdue, no movement, on you... */
  chips: { label: string; tone: 'bad' | 'warn' | 'blue' | 'dim' }[];
  /** The references it carries, as short labels with a href when followable. */
  links: { label: string; href: string | null }[];
  /** A parent's children: `done/total`. */
  rollup: string | null;
  /** When the row last moved, in epoch seconds: the page ticks it live. */
  updatedAt: number;
}

/** One lane: a state, its cards in rank order. */
export interface LaneView {
  key: TaskStatus;
  name: string;
  count: number;
  cards: BoardCardView[];
}

/** Where a drag put a card, until the server's own snapshot catches up. */
export interface PendingMove {
  to: TaskStatus;
  /** The row it should sit above in the target lane, or nothing for the end. */
  before: string | null;
  /** When the edit went, in epoch millis: the page's own bound on waiting. */
  at: number;
}

/**
 * The lanes with the reader's own drags already applied. A drop is
 * immediate on the page - a card that waits seconds for the server's
 * snapshot reads as a drag that failed - so the card is placed here at
 * once and the wire overwrites it when it lands.
 */
export function applyMoves(lanes: LaneView[], moves: Record<string, PendingMove>): LaneView[] {
  const cards = new Map(lanes.flatMap((lane) => lane.cards).map((card) => [card.id, card]));
  return lanes.map((lane) => {
    const staying = lane.cards.filter((card) => (moves[card.id]?.to ?? card.status) === lane.key);
    const arriving = Object.entries(moves)
      .filter(([id, move]) => move.to === lane.key && cards.get(id)?.status !== lane.key)
      .map(([id, move]) => ({ card: cards.get(id), move }))
      .filter(
        (entry): entry is { card: BoardCardView; move: PendingMove } => entry.card !== undefined,
      )
      .filter((entry) => !lane.cards.some((card) => card.id === entry.card.id));
    for (const { card, move } of arriving) {
      const at =
        move.before === null ? staying.length : staying.findIndex((c) => c.id === move.before);
      staying.splice(at === -1 ? staying.length : at, 0, card);
    }
    // A card re-ordered inside its own lane takes the drop position too.
    for (const [id, move] of Object.entries(moves)) {
      if (move.to !== lane.key || move.before === null) continue;
      const from = staying.findIndex((card) => card.id === id);
      if (from === -1) continue;
      const [card] = staying.splice(from, 1);
      if (card === undefined) continue;
      const at = staying.findIndex((c) => c.id === move.before);
      staying.splice(at === -1 ? staying.length : at, 0, card);
    }
    return { ...lane, count: staying.length, cards: staying };
  });
}

/**
 * Whether the wire's own answer has the moved row where the drop put it.
 *
 * **The state alone is not enough.** A re-order inside a lane keeps the row's
 * status, so a status-only test called every re-order landed on the next tick
 * and the card snapped back to the order the server still held; the PLACE is
 * what the drop asked for, so the place is what confirms it. A row the wire
 * does not carry at all is not landed either - the core may have archived it.
 */
export function landed(rows: BoardRow[], id: string, want: PendingMove): boolean {
  if (!rows.some((entry) => entry.task.id === id && entry.task.status === want.to)) return false;
  const lane = rows
    .filter((entry) => entry.task.status === want.to)
    .sort((a, b) => {
      const rankA = a.task.rank ?? Number.MAX_SAFE_INTEGER;
      const rankB = b.task.rank ?? Number.MAX_SAFE_INTEGER;
      if (rankA !== rankB) return rankA - rankB;
      return a.task.created_at.secs_since_epoch - b.task.created_at.secs_since_epoch;
    })
    .map((entry) => entry.task.id);
  const at = lane.indexOf(id);
  return at !== -1 && (lane[at + 1] ?? null) === want.before;
}

/** One row waiting on the user, as the strip draws it. */
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
  /** The header's own read of the board. */
  read: { onYou: number; rows: number; running: number };
  lanes: LaneView[];
  waiting: WaitingView[];
  /** Whether the snapshot carries this project at all. */
  known: boolean;
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
function chipsOf(task: Task, marks: Marks, updatedSecsAgo: number): BoardCardView['chips'] {
  const chips: BoardCardView['chips'] = [];
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
function linksOf(task: Task): BoardCardView['links'] {
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

/** The lane a card belongs to: one lane per state, named as the state is. */
function laneOf(status: TaskStatus): LaneView['key'] {
  return status;
}

/** One card, resolved. */
function cardView(row: BoardRow, epic: string | null): BoardCardView {
  const task = row.task;
  const estimate = task.estimate;
  const ratio =
    estimate === null || estimate.secs <= 0 ? 0 : Math.min(1, row.worked_secs / estimate.secs);
  const tone =
    estimate === null || estimate.secs <= 0
      ? null
      : row.worked_secs > estimate.secs * 1.5
        ? 'bad'
        : row.worked_secs > estimate.secs
          ? 'over'
          : null;
  return {
    id: task.id,
    subject: task.subject,
    display:
      task.status === 'in_progress' && task.active_form !== null && task.active_form !== ''
        ? task.active_form
        : task.subject,
    epic,
    status: task.status,
    owner: task.owner === null ? null : task.owner.label,
    initials: (task.owner === null || task.owner.label === '' ? '?' : task.owner.label)
      .slice(0, 1)
      .toUpperCase(),
    worked: fmtSecs(row.worked_secs),
    estimate: estimate === null ? null : estimate.words,
    ratio,
    tone,
    chips: chipsOf(task, row.marks, row.updated_secs_ago),
    links: linksOf(task),
    rollup: row.rollup === null ? null : `${row.rollup[0]}/${row.rollup[1]}`,
    updatedAt: task.updated_at.secs_since_epoch,
  };
}

/**
 * `(org, project)`'s board: cards in lanes by state, rank order inside each,
 * and the rows waiting on the user in the strip above. A project the snapshot
 * does not carry is UNKNOWN rather than empty - a route can be addressed
 * before the first frame lands, and a board with an active create line for a
 * project this forge does not hold invites a row against nothing.
 */
export function boardView(wire: HomeWire, org: string, project: string): BoardView {
  const row = wire.projects.find(
    (entry) => entry.project.org === org && entry.project.name === project,
  );
  // The fleet row is where the server's own count of the board lives.
  const fleet = wire.fleet.find((entry) => entry.project === project);
  const lanes = (
    [
      ['in_progress', 'In progress'],
      ['waiting', 'Waiting'],
      ['pending', 'Ready'],
      ['completed', 'Completed'],
      ['failed', 'Failed'],
      ['canceled', 'Canceled'],
    ] as const
  ).map(([key, name]) => ({ key, name, count: 0, cards: [] as BoardCardView[] }));
  if (row === undefined) {
    return {
      name: project,
      org,
      read: { onYou: 0, rows: 0, running: 0 },
      lanes,
      waiting: [],
      known: false,
      empty: false,
    };
  }
  // The rank order the server would dispatch from: rank first, then
  // creation, so an unranked row sorts after the ranked ones.
  const ordered = [...row.rows].sort((a, b) => {
    const rankA = a.task.rank ?? Number.MAX_SAFE_INTEGER;
    const rankB = b.task.rank ?? Number.MAX_SAFE_INTEGER;
    if (rankA !== rankB) return rankA - rankB;
    return a.task.created_at.secs_since_epoch - b.task.created_at.secs_since_epoch;
  });
  const subjectOf = (parent: string | null): string | null =>
    parent === null ? null : (ordered.find((e) => e.task.id === parent)?.task.subject ?? null);
  for (const entry of ordered) {
    const target = lanes.find((lane) => lane.key === laneOf(entry.task.status));
    if (target === undefined) continue;
    target.cards.push(cardView(entry, subjectOf(entry.task.parent)));
  }
  for (const lane of lanes) lane.count = lane.cards.length;
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
  return {
    name: project,
    org,
    read: {
      onYou: fleet?.waiting_on_user ?? 0,
      rows: row.rows.length,
      running: lanes[0]?.count ?? 0,
    },
    lanes,
    waiting,
    known: true,
    empty: row.rows.length === 0,
  };
}

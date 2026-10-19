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
  /** The parent's id, so the child's pill can jump to it. */
  epicId: string | null;
  /**
   * A parent's own tasks, in the queue's order, each labelled with its
   * lane's word - the list the row's page draws, each row a door that lands
   * on its card.
   */
  children: { id: string; label: string }[];
  status: TaskStatus;
  owner: string | null;
  /** The owner picker's initial: `lead` is one person here, so one letter. */
  initials: string;
  /** The measure line's own words: worked, against the estimate where it has one. */
  time: string;
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
  /**
   * Whether the drop asked for a POSITION. A drag does on both its paths -
   * a re-order inside a lane, and a move across one carrying its `place` -
   * while a keyboard lane step asks only for the state, so its place is not
   * demanded when the wire answers.
   */
  ranked: boolean;
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
 * **What the drop ASKED FOR is what confirms it.** A re-order inside a lane
 * keeps the row's status, so a status-only test called every re-order landed
 * on the next tick and the card snapped back to the order the server still
 * held - the place is what that drop asked for. A drag across a lane carries
 * its place too, so both ask for it; a keyboard lane step asks for the state
 * alone, so the place is NOT demanded there - demanding it would call the
 * step a failure the core never made. A row the wire does not carry at all is
 * not landed either - the core may have archived it.
 */
export function landed(rows: BoardRow[], id: string, want: PendingMove): boolean {
  if (!rows.some((entry) => entry.task.id === id && entry.task.status === want.to)) return false;
  if (!want.ranked) return true;
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
  /** The card's title: the wait's own detail, else the state's sentence. */
  ask: string;
  /** The row's references, as doors, beside the ask. */
  links: { label: string; href: string | null }[];
  /** The artifact a look points at, when the row carries a followable one. */
  artifact: { label: string; href: string } | null;
  /** How long it has waited. */
  age: string;
  /**
   * Whether the row is a ROOT. Only a root's completion closes a tree, so
   * only a root can be refused for what its children are doing.
   */
  root: boolean;
  /**
   * How many of the row's children are not TERMINAL - the core's own word for
   * `Completed | Failed | Canceled`, and the set its close rule counts. A
   * root cannot complete while any child is non-terminal, so an approve
   * offered over one would be a press the core refuses; the strip says so
   * instead, and withholds the control.
   */
  open: number;
  /** The row's children as the server counted them: `completed/total`. */
  rollup: string | null;
}

/**
 * Whether a status is an end. **The core's own rule** (`is_terminal` in
 * `forge-workspace/src/tasks.rs`), mirrored because the strip has to know
 * which children a root's close would still be refused over - a failed or
 * canceled child is not a child the close waits for.
 */
function terminal(status: TaskStatus): boolean {
  return status === 'completed' || status === 'failed' || status === 'canceled';
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

/**
 * Whether a row's wait is the READER's: a decision, or a look park on a row
 * carrying its own `verify: 'user'`. The gate is minted on the row, never
 * inherited from its epic (#2423), so a verification wait an inherited gate
 * left behind is a plain wait, not an ask on the reader.
 */
export function waitOnUser(task: Task): boolean {
  const wait = task.waiting_on;
  if (wait === null) return false;
  return wait.verification ? task.verify === 'user' : wait.kind === 'decision';
}

/** The chips a row's marks and its wait imply, in reading order. */
function chipsOf(task: Task, marks: Marks, updatedSecsAgo: number): BoardCardView['chips'] {
  const chips: BoardCardView['chips'] = [];
  if (task.waiting_on !== null) {
    const wait = task.waiting_on;
    chips.push(
      waitOnUser(task)
        ? { label: 'waiting on you', tone: 'warn' }
        : wait.kind === 'dependency'
          ? { label: `on ${wait.on ?? 'another row'}`, tone: 'warn' }
          : wait.kind === 'resource'
            ? { label: 'waiting on a resource', tone: 'warn' }
            : { label: 'waiting', tone: 'warn' },
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
  if ((task.attested ?? null) !== null) chips.push({ label: 'lead attested', tone: 'blue' });
  if (marks.to_close) chips.push({ label: 'to close', tone: 'blue' });
  if (task.reserved === 'user') chips.push({ label: 'reserved for you', tone: 'dim' });
  if (task.attempt > 1) chips.push({ label: `attempt ${task.attempt}`, tone: 'dim' });
  return chips;
}

/** The links a row shows: short labels, with a href when the target is a URL. */
/**
 * The artifact a look points at, where the row carries one: the first
 * followable PR or path link. A look's ask usually names a mock, an
 * approach or a view, and the reader should reach it in one press.
 */
/**
 * The sentence a wait's card says when it states no ask of its own: the
 * state, in the reader's terms. A seeded or legacy wait keeps this, and a
 * wait with a detail leads with the detail instead (#2423 look).
 */
export function waitSentence(task: Task): string {
  const wait = task.waiting_on;
  if (wait === null) return 'Waiting.';
  if (waitOnUser(task) && wait.verification) {
    return 'This row is finished and waits on your approval.';
  }
  if (waitOnUser(task) && wait.kind === 'decision') return 'A decision is waiting on you.';
  if (wait.kind === 'dependency') return `Waiting on ${wait.on ?? 'another row'}.`;
  if (wait.kind === 'resource') return 'Waiting on a resource.';
  return 'Waiting.';
}

export function artifactOf(task: Task): { label: string; href: string } | null {
  const link = task.links.find(
    (candidate) =>
      (candidate.kind === 'pr' || candidate.kind === 'path') && candidate.target.startsWith('http'),
  );
  if (link === undefined) return null;
  return { label: link.label ?? shortTarget(link.target), href: link.target };
}

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
function cardView(
  row: BoardRow,
  epic: string | null,
  children: { id: string; label: string }[],
): BoardCardView {
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
    epicId: task.parent,
    children,
    status: task.status,
    owner: task.owner === null ? null : task.owner.label,
    initials: (task.owner === null || task.owner.label === '' ? '?' : task.owner.label)
      .slice(0, 1)
      .toUpperCase(),
    time:
      estimate === null
        ? `worked ${fmtSecs(row.worked_secs)}`
        : `worked ${fmtSecs(row.worked_secs)} / ${estimate.words}`,
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
  // The drawn order, which the keyboards' left/right walks: the pool of
  // everything not yet started leads, then the lane that wants the reader,
  // then the work. `pending` reads Backlog - the state never had a name of
  // its own on this page, and the pool is what it is.
  const lanes = (
    [
      ['pending', 'Backlog'],
      ['waiting', 'Waiting'],
      ['in_progress', 'In progress'],
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
  const laneName = new Map(lanes.map((lane) => [lane.key, lane.name]));
  const childrenOf = (id: string): { id: string; label: string }[] =>
    ordered
      .filter((e) => e.task.parent === id)
      .map((e) => ({
        id: e.task.id,
        label: `${laneName.get(e.task.status) ?? e.task.status} \u{b7} ${e.task.subject}`,
      }));
  for (const entry of ordered) {
    const target = lanes.find((lane) => lane.key === laneOf(entry.task.status));
    if (target === undefined) continue;
    target.cards.push(cardView(entry, subjectOf(entry.task.parent), childrenOf(entry.task.id)));
  }
  for (const lane of lanes) lane.count = lane.cards.length;
  const waiting = ordered
    .filter((entry) => entry.task.status === 'waiting' && waitOnUser(entry.task))
    .map((entry) => ({
      id: entry.task.id,
      subject: entry.task.subject,
      owner: entry.task.owner === null ? null : entry.task.owner.label,
      verification: entry.task.waiting_on?.verification === true,
      detail: entry.task.waiting_on?.detail ?? null,
      ask: entry.task.waiting_on?.detail ?? waitSentence(entry.task),
      links: linksOf(entry.task),
      artifact:
        entry.task.waiting_on?.verification === true && waitOnUser(entry.task)
          ? artifactOf(entry.task)
          : null,
      age: fmtSecs(entry.updated_secs_ago),
      root: entry.task.parent === null,
      // Counted from the rows themselves, NOT from the rollup: the rollup's
      // first number counts completed children, and a failed or canceled
      // child is one the close no longer waits for.
      open: ordered.filter(
        (child) => child.task.parent === entry.task.id && !terminal(child.task.status),
      ).length,
      rollup: entry.rollup === null ? null : `${entry.rollup[0]}/${entry.rollup[1]}`,
    }));
  return {
    name: project,
    org,
    read: {
      onYou: fleet?.waiting_on_user ?? 0,
      rows: row.rows.length,
      running: lanes.find((lane) => lane.key === 'in_progress')?.count ?? 0,
    },
    lanes,
    waiting,
    known: true,
    empty: row.rows.length === 0,
  };
}

/**
 * The session page's reads gathered into the shape its markup wants - the
 * client half of the gather `crates/forge-web/src/session.rs` does across its
 * own render functions.
 *
 * Pure, so a test can build a session by hand. Nothing here recomputes a state
 * the server already decided: a row's lifecycle, a monitor's status and a
 * server's connection state all arrive in the record, and a row appears only
 * when there is something behind it - a row that is always there says nothing
 * when it is empty.
 *
 * Some rows do not read the seat's own record. A project's tasks and its
 * schedules are keyed by PROJECT on the home's snapshot, and the home is the
 * subscription the shell already holds for the rest of the app.
 */

import type { Snippet } from 'svelte';

import {
  artifactLabel,
  availableVersion,
  chipFor,
  elapsedLabel,
  gateLine,
  placeOf,
  projectRows,
  stateOf,
  whenOf,
} from '../home/view';
import type { Row, RowState } from '../home/view';
import { FORGE_COMMANDS } from '../composer/forge-commands';
import type { ComposerRecord } from '../composer/view';
import { CLIENT_VERSION, PROTOCOL_VERSION } from '../protocol';
import { hrefForSlot } from '../routes';
import type { Connection } from '../socket';
import type { AgentRow, CronEntry, HomeWire, Lifecycle, ProjectWire, Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import type {
  FileStatusWire,
  McpServer,
  MonitorRecord,
  SessionHeader,
  SessionRecord,
} from './wire';

/** The facts the header states, and the class the mode's chip carries. */
export interface Facts {
  /** The occupant's id, or `null` when there is no occupant to name. */
  sessionId: string | null;
  model: string;
  effort: string;
  mode: { wire: string; klass: string } | null;
  percent: number | null;
}

/** What the rail draws under one heading. */
export interface RailGroup {
  heading: string;
  /** `needs` carries the heading's own colour, which is the sheet's rule. */
  klass: string;
  /**
   * How many rows this heading folds away, or `null` when it folds nothing.
   *
   * One field rather than a flag beside a number, so a heading cannot claim to
   * fold and hide nothing: a folded section with no count reads as an empty
   * one, which is the whole reason the count is drawn on it.
   */
  hidden: number | null;
  /**
   * The strongest signal among the rows behind this heading, drawn on the
   * heading itself - or `null` when they carry none, since the quiet ring a
   * sleeping row wears says nothing the heading has not already said (#1868).
   */
  mark: string | null;
  /**
   * Whether the seat the page is showing is one of the rows behind this
   * heading, which is what a fold opens itself on: arriving on a sleeping seat
   * would otherwise draw the marked row inside a closed fold.
   */
  holds: boolean;
  projects: RailProject[];
}

/** One project in the rail: its own row, its workers, and why it is here. */
export interface RailProject {
  name: string;
  org: string;
  /**
   * The LABEL of the row the page is showing, or `null` when it is showing
   * another project.
   *
   * A label rather than a flag, because the mark is on one row: a project's
   * own row is its lead and a worker's row is its own label, so a row compares
   * against this and exactly one of them matches.
   */
  shown: string | null;
  /** How long since it last wrote, drawn only when nothing is running. */
  age: string;
  asleep: boolean;
  row: Row;
  /** The workers it draws, which are the ones awake. */
  workers: Row[];
  /**
   * The sleeping workers one row of theirs hides, empty when there are none:
   * a reader working in a live project is not working in the seats beside it
   * that have gone to sleep. Carried whole rather than as a count, because the
   * row that hides them is the row that opens them.
   */
  sleeping: Row[];
  why: { line: string; bad: boolean } | null;
}

/** One changed file as a strip row draws it. */
export interface StripFile {
  path: string;
  status: FileStatusWire;
  added: number;
  removed: number;
}

/** One file list's rows and totals, as a panel section draws them. */
export interface GitStats {
  files: StripFile[];
  totalFiles: number;
  totalAdded: number;
  totalRemoved: number;
}

/** One row the palette can land on. */
export interface PaletteRow {
  id: string;
  kind: 'seat' | 'command' | 'doing';
  /** The word the row leads with. */
  label: string;
  /** The quiet line beside it: the seat's state, or what the action does. */
  detail: string;
  /** Where a seat or the home row goes, as a real href. */
  href?: string;
  /** What a command row sends, exactly as the composer sends it. */
  text?: string;
  /** Which doing a doing row runs. */
  doing?: 'peek' | 'copy' | 'close';
  /** The current project's lead, which is where the cursor starts. */
  lead?: boolean;
  /** A seat row's dot class, in the home's own vocabulary. */
  mark?: string;
}

export interface PaletteSection {
  title: string;
  rows: PaletteRow[];
}

/** A seat's state in the palette's two or three words. */
function paletteWord(row: AgentRow, unseen: SessionSlot[]): string {
  if (row.pending !== null) return 'needs you';
  const state = stateOf(row, unseen);
  switch (state.kind) {
    case 'failed-turn':
      return 'failed';
    case 'unseen':
      return 'finished';
    case 'never-started':
      return 'never started';
    case 'lifecycle':
      switch (state.lifecycle) {
        case 'Attention':
          return 'needs you';
        case 'AuthRequired':
          return 'sign-in needed';
        case 'Failed':
          return 'failed';
        case 'Running':
          return 'working';
        case 'Spawning':
          return 'starting';
        case 'Sleeping':
        case 'LoggedOut':
          return 'asleep';
        default:
          return 'idle';
      }
  }
}

/**
 * **The palette's rows**: the fleet grouped its own way - the seats that
 * want a person, the ones working, the ones asleep - then forge's commands,
 * then the doings. Every seat's state is searchable in its detail line, the
 * current project's lead carries `lead` (where the cursor starts, so Cmd+K
 * then Enter lands on it), and a command row's `text` is exactly what the
 * composer sends.
 */
export function paletteRows(wire: HomeWire, slot: SessionSlot): PaletteSection[] {
  const unseen = wire.unseen;
  // **One rule with the rail's own ranker**: the palette and the rail draw
  // the same seat two layers apart, so a seat that reads working on the rail
  // cannot read asleep here. rankOf's 0/1/2 is needs-person / working /
  // asleep.
  const rank = (row: AgentRow): number => rankOf(stateOf(row, unseen), row.pending);
  const wants = (row: AgentRow): boolean => rank(row) === 0;
  const working = (row: AgentRow): boolean => rank(row) === 1;
  const seat = (row: AgentRow): PaletteRow => ({
    id: `${row.slot.org}/${row.slot.project}/${row.label}`,
    kind: 'seat',
    label: row.slot.label,
    detail: `${row.slot.project} \u{b7} ${paletteWord(row, unseen)}`,
    href: hrefForSlot(row.slot),
    // A project's identity is (org, name): two orgs sharing a project name
    // would otherwise both wear the chip, and Enter would land in the wrong
    // one.
    lead:
      row.slot.org === slot.org && row.slot.project === slot.project && row.slot.label === 'lead',
    mark: railMark(stateOf(row, unseen)),
  });
  const seats = wire.agents;
  const sections: PaletteSection[] = [
    { title: 'needs you', rows: seats.filter(wants).map(seat) },
    {
      title: 'working',
      rows: seats.filter((row) => !wants(row) && working(row)).map(seat),
    },
    {
      title: 'asleep',
      rows: seats.filter((row) => !wants(row) && !working(row)).map(seat),
    },
    {
      title: 'commands',
      // The composer's own table, not a second copy of it: a command the box
      // offers is a command the palette offers, or neither.
      rows: FORGE_COMMANDS.map((command) => ({
        id: `cmd-${command.name.replace('/', '')}`,
        kind: 'command' as const,
        label: command.name,
        detail: command.description,
        text: command.name,
      })),
    },
    {
      title: 'doings',
      rows: [
        {
          id: 'do-peek',
          kind: 'doing',
          label: 'peek at the fleet',
          detail: 'the projects rail, over this page',
          doing: 'peek',
        },
        {
          id: 'do-home',
          kind: 'doing',
          label: 'go home',
          detail: 'every project and every seat',
          href: '/',
        },
        {
          id: 'do-copy',
          kind: 'doing',
          label: 'copy the session id',
          detail: "this seat's occupant",
          doing: 'copy',
        },
        {
          id: 'do-close',
          kind: 'doing',
          label: 'close this seat',
          detail: 'the session ends; the row stays',
          doing: 'close',
        },
      ],
    },
  ];
  return sections.filter((section) => section.rows.length > 0);
}

/** The projects chip's state: the seats that want a person, fleet-wide. */
export interface ChipState {
  /** `one` goes straight to that seat; everything else goes to the home. */
  state: 'one' | 'many' | 'failed' | 'none';
  count: number;
  /** Where the chip's own click lands, as a real href. */
  href: string;
  /** The words a screen reader hears, which is also the title. */
  label: string;
}

/**
 * **The chip counts seats that want a PERSON**: a question, a permission, a
 * died turn - never a mere change. Exactly one, and it is not an error, is
 * the one case its click goes straight to that seat; several, or any failure
 * among them, goes to the home page so the reader picks; none goes home too,
 * from a quiet `projects` label.
 */
export function chipState(wire: HomeWire | null): ChipState {
  const rows = wire === null ? [] : wire.agents;
  const wanting = rows.filter((row) => row.pending !== null || row.failed_turn !== null);
  const failed = wanting.some((row) => row.failed_turn !== null);
  if (wanting.length === 0) {
    return { state: 'none', count: 0, href: '/', label: 'projects' };
  }
  const said = wanting.length === 1 ? '1 seat needs you' : `${wanting.length} seats need you`;
  if (failed) {
    // The cross is aria-hidden and the tone is colour alone, so the failure
    // must reach the accessible name too.
    return {
      state: 'failed',
      count: wanting.length,
      href: '/',
      label: `${said}, one failed`,
    };
  }
  const label = said;
  const only = wanting[0];
  if (wanting.length === 1 && only !== undefined) {
    return { state: 'one', count: 1, href: hrefForSlot(only.slot), label: '1 seat needs you' };
  }
  return { state: 'many', count: wanting.length, href: '/', label };
}

/** The seat's working tree, as the strip's row draws it. */
export interface GitStrip {
  /** The toggle's own line: the branch, how far it runs, what moved and
   *  whether the tree is dirty - or where the tree is not. */
  label: string;
  /** What the tree IS - the project's, or a worker's worktree - which is the
   *  first thing a reader looking at a fleet needs to know. */
  head: string;
  /** The branch's chain ahead of its default, when it has one: each commit
   *  with the files it changed and when it landed, and the range's own
   *  totals. */
  ahead: {
    count: number;
    base: string | null;
    commits: { sha: string; subject: string; stats: GitStats | null; time: number }[];
    stats: GitStats | null;
  } | null;
  /** The uncommitted layer, with its marks and counts. */
  uncommitted: GitStats | null;
  /** The open pull request, whether it is a draft, and what it closes. */
  pr: { number: number; url: string; draft: boolean; closes: string } | null;
  /** Why the tree could not be read, when it could not. */
  gate: string | null;
}

/** A file's mark: the letter the terminal's rail draws, and its tone. */
export function markOf(status: FileStatusWire): { letter: string; klass: string } {
  switch (status) {
    case 'added':
      return { letter: 'A', klass: 'ok' };
    case 'deleted':
      return { letter: 'D', klass: 'bad' };
    case 'renamed':
      return { letter: 'R', klass: 'mark' };
    case 'copied':
      return { letter: 'C', klass: 'mark' };
    case 'typechange':
      return { letter: 'T', klass: 'mark' };
    case 'unmerged':
      return { letter: '!', klass: 'bad' };
    case 'untracked':
      return { letter: 'U', klass: 'warn' };
    case 'modified':
      return { letter: 'M', klass: 'mark' };
  }
}

/** One task, as the strip's row draws it. */
export interface TaskStripRow {
  /** The task's own id, which is what the row is keyed by. */
  id: string;
  /** Where the task is, which the row's own mark draws. */
  status: Task['status'];
  /** The row's own text: the active form while one runs, else the subject. */
  display: string;
  subject: string;
  owner: string | null;
  /** The facts beside the owner: how far along, what it produced, how long. */
  meta: string;
}

/** One MCP server, as the strip's row draws it. */
export interface McpRow {
  /** The server's name, which is what a row is keyed by. */
  name: string;
  /** The line the row leads with: the name, and the scope it is configured in. */
  k: string;
  /** What the row says beside it: tool count, or why it is not up. */
  v: string;
  /** The tools the server offers when it is connected, as its status depth. */
  tools: string[];
  /** What backs a subprocess-backed server, when its config names one: the
   *  command it runs, or the URL it reaches. */
  command: string | null;
  /** The failure reason a Failed read carries, shown as the detail line. */
  reason: string | null;
  /**
   * Whether this row stands for the READ rather than a server: an empty read
   * that failed draws as itself, and a toggle must not count it as a server
   * the session has.
   */
  synthetic: boolean;
}

/**
 * What the conversation column is handed, and what the chat's own task writes
 * its component against.
 *
 * **The conversation is not here, and that is deliberate.** The chat pages for
 * its own history with `more` and follows the live tail off `chat_appended`, so
 * handing it the record's turns would re-cross the conversation on every
 * re-read - which is the cost a virtualised list exists to avoid, and it would
 * take the reader's place with it. The rest is the shell's own read, and the
 * chat has no other way to get it.
 */
export interface ConversationProps {
  /** The directory the seat's calls are named against. */
  /** Whether a seat is behind this page at all. */
  waking: boolean;
  /**
   * Whether the core is bringing the seat up. Its own state beside `waking`:
   * both draw the waking line, and the column reads the conversation only
   * once neither is true (#1712's follow-up).
   */
  spawning: boolean;
  /** Why it is not running, when the roster says. */
  reason: string | null;
  slot: SessionSlot;
  connection: Connection;
  /**
   * The waiting prompts, drawn at the column's foot above the pinned row.
   *
   * The queue's DATA is the page's - it owns the record - but its PLACE is
   * here, between the turns and the strip: what is waiting reads against
   * what is running, and the strip stays right above the box. A snapshot
   * rather than props, because the chat must not reach for the record.
   */
  queue?: Snippet;
}

/** What the composer is handed, and what its own task writes against. */
export interface ComposerProps {
  /**
   * The record the box reads, which is the composer's own narrow type rather
   * than the whole session record: the page may hand an empty one while a
   * seat's first read is still landing, and the box draws from the same fields
   * either way.
   */
  record: ComposerRecord;
  slot: SessionSlot;
  /** The seat behind the page, which its blocked states read. */
  seat: SeatState;
  connection: Connection;
}

/**
 * The state mark the rail and the header draw: the core's lifecycle plus the
 * two promotions the home makes over it.
 *
 * Its own mapping rather than the home's, because the two are not the same
 * shape: a home row's mark is a class on the row and a dot shape inside it,
 * while the rail and the header put the state's own name on a bare dot.
 */
export function railMark(state: RowState): string {
  if (state.kind === 'unseen') return 'unseen';
  // A failed turn draws the failure mark, the terminal's own answer: its
  // cross replaces the state glyph until the seat is opened.
  if (state.kind === 'failed-turn') return 'failed';
  if (state.kind === 'never-started') return 'off';
  switch (state.lifecycle) {
    case 'Running':
    case 'Spawning':
      return 'live';
    case 'Idle':
      return 'idle';
    case 'Attention':
      return 'needs';
    // Sign-in needed is a failure a person has to act on, and neither the
    // rail nor the header has an auth shape of its own.
    case 'AuthRequired':
    case 'Failed':
      return 'failed';
    case 'Sleeping':
    case 'LoggedOut':
      return 'off';
  }
}

/**
 * The mark a rail row wears: a seat this client has just closed reads as
 * settling until the core finishes shutting it down (#1712), and every other
 * row wears its own state's mark. Every drawer of a row goes through this -
 * the rail's lead row, a project's sleeping rows and the summary that folds
 * them - so the three cannot drift apart.
 */
export function rowMark(row: Row, closing: (slot: SessionSlot) => boolean): string {
  return closing(row.slot) ? 'off settling' : railMark(row.state);
}

/**
 * The mark a fold's heading wears: the settling ring, when a seat this client
 * has just closed is behind it, and nothing otherwise.
 *
 * Nothing else can be there: every mark the rail ranks above settling belongs
 * to a row that has already been lifted out of the fold's own section, so a
 * quiet ring says nothing the heading has not said and the rest cannot arrive.
 */
export function foldMark(marks: readonly string[]): string | null {
  return marks.includes('off settling') ? 'off settling' : null;
}

/**
 * How much of the fleet is up: the projects whose own seat is running, of the
 * projects the roster declares.
 *
 * Counted from the seats rather than from the rows the rail draws, so a
 * project filtered out of a group is still counted.
 */
export function fleetCount(home: HomeWire): string {
  const live = home.projects.filter((row) =>
    home.agents.some(
      (agent) =>
        agent.slot.org === row.project.org &&
        agent.slot.project === row.project.name &&
        agent.slot.label === 'lead',
    ),
  ).length;
  return `${live} live / ${home.projects.length}`;
}

/** What the header and the conversation column say about the seat behind the page. */
export interface SeatState {
  /** Whether anything is running behind this page at all. */
  waking: boolean;
  /**
   * The core's own lifecycle, or `null` for a seat the roster does not name.
   *
   * `waking` cannot stand in for it: a composer has to replace its box and say
   * WHY for a seat that is spawning, one that failed and one that needs
   * signing in, and a boolean tells none of the three from the others.
   */
  lifecycle: Lifecycle | null;
  /** Why it is not running, when the roster says. */
  reason: string | null;
  /**
   * How many prompts this seat is holding, the one on screen included.
   *
   * The composer's dock states how many wait behind the prompt it draws, and
   * the count is the core's: a view that kept its own queue would be a second
   * decider about a queue the core arbitrates.
   */
  pendingDepth: number;
  mark: string;
  /** What the header calls the seat: a lead is its project, a worker its label. */
  name: string;
}

/**
 * The seat the page is showing, as the roster holds it.
 *
 * A seat the roster does not name is one nothing has started, which is a state
 * this page draws rather than a page it refuses: the columns are as real for a
 * seat with nothing behind it as for one that is up, and only the chat column
 * tells the two apart.
 */
/**
 * Whether the seat's name needs its org to be unambiguous.
 *
 * **The org shows only where the fleet makes the name ambiguous.** A lead's
 * name is its project's and a worker's is its own label ({@link seatState}),
 * and neither is unique across orgs - two `forge` projects under different
 * orgs share one word - so the header qualifies the name exactly then, and
 * nowhere else (#1707).
 */
export function orgNeeded(home: HomeWire, slot: SessionSlot): boolean {
  const name = slot.label === 'lead' ? slot.project : slot.label;
  return home.projects.some(
    (entry) => entry.project.name === name && entry.project.org !== slot.org,
  );
}

export function seatState(home: HomeWire, slot: SessionSlot): SeatState {
  const row = home.agents.find(
    (agent) =>
      agent.slot.org === slot.org &&
      agent.slot.project === slot.project &&
      agent.slot.label === slot.label,
  );
  return {
    waking: row === undefined,
    lifecycle: row?.lifecycle ?? null,
    reason: row?.reason ?? null,
    // A seat the roster does not name is holding nothing, so the count is the
    // one prompt the composer may be drawing rather than zero.
    pendingDepth: row?.pending_depth ?? 1,
    mark: railMark(row === undefined ? { kind: 'never-started' } : stateOf(row, home.unseen)),
    // A lead's row is its project, the way the home names it; a worker's is
    // its own label.
    name: slot.label === 'lead' ? slot.project : slot.label,
  };
}

/** One monitor, as the strip's row draws it. */
export interface MonitorStripRow {
  /** The monitor's own tool_use_id, which is what the row is keyed by. */
  id: string;
  running: boolean;
  /** Whether it ENDED well: `completed` only, never `stopped` or `timed_out`. */
  completed: boolean;
  name: string;
  label: string;
  command: string;
}

/**
 * The four facts, and the class the permission mode's chip carries.
 *
 * A model with no long name draws its resolved id: the CLI reports an empty
 * display name for a model it has no catalogue entry for, and an empty cell
 * would read as a session with no model.
 */
export function headerFacts(header: SessionHeader): Facts {
  const model = header.model;
  return {
    sessionId: header.session_id,
    model:
      model === null
        ? '\u{2014}'
        : model.display_name_long === ''
          ? model.resolved_id
          : model.display_name_long,
    effort: header.effort,
    mode:
      header.permission_mode === null
        ? null
        : { wire: header.permission_mode, klass: permClass(header.permission_mode) },
    percent: header.context.percent,
  };
}

/**
 * The compaction figure the header draws beside the context bar, or `null` when
 * the session has nothing to say.
 *
 * **The count is the conversation's, not the header's**: a boundary is a
 * `compact_boundary` row in the transcript, so it rides `conversation` on the
 * record. `null` at zero is the terminal's rule - a `0 compactions` on every
 * fresh session is noise on a row that already carries five facts.
 */
export function compactionFigure(count: number): string | null {
  if (count === 0) return null;
  return count === 1 ? '1 compaction' : `${count} compactions`;
}

/**
 * What the copy control's click did, or what stands in the way of one.
 *
 * `no-clipboard` and `failed` are the two failures kept apart because they are
 * different problems for the reader: the first is the page's origin, the
 * second is a write the OS refused.
 */
export type CopyOutcome = 'ready' | 'copied' | 'failed' | 'no-clipboard';

/**
 * What the control is for: its accessible name, and the reason a state other
 * than the ready one is showing.
 *
 * The name is spelt out rather than left as the mark, so a reader who cannot
 * see the glyph beside the id still knows what the click does and, when it
 * changed, why.
 */
export function copyReason(outcome: CopyOutcome): string {
  switch (outcome) {
    case 'ready':
      return 'copy the whole session id';
    case 'copied':
      return 'copied, the whole id is on the clipboard';
    case 'failed':
      return 'copy failed, the clipboard refused the write';
    case 'no-clipboard':
      return 'copy needs https, this page has no clipboard to write to';
  }
}

/**
 * The class the mode's chip carries, so the colour says how much the session
 * may do without being asked.
 */
function permClass(mode: string): string {
  if (mode === 'auto' || mode === 'acceptEdits') return 'auto';
  if (mode === 'plan') return 'plan';
  if (mode === 'bypassPermissions') return 'bypass';
  return '';
}

/**
 * Where a row sits in the rail. The three groups are also the order they read
 * in, so the rank is the order and the array index both.
 */
export function rankOf(state: RowState, pending: 'question' | 'permission' | null): number {
  // An ask outranks the lifecycle: a session holds a prompt while the core
  // still calls it idle, and what it is waiting on is a person.
  if (pending !== null) return 0;
  if (state.kind === 'unseen') return 1;
  // A failed turn is the seat's own version of a needed person, so it ranks
  // with the attention states rather than with the completions.
  if (state.kind === 'failed-turn') return 0;
  if (state.kind === 'never-started') return 2;
  switch (state.lifecycle) {
    case 'Attention':
    case 'Failed':
    case 'AuthRequired':
      return 0;
    case 'Sleeping':
    case 'LoggedOut':
      return 2;
    default:
      return 1;
  }
}

/**
 * The line a failed row draws under itself: the core's recorded reason, or the
 * fallback for a failure it left no text for.
 *
 * The fallback word follows the terminal's split: a failed row it holds no
 * text for reads "spawn failed" there (its sub-row exists only on a failed
 * worker), while a seat waiting on sign-in draws no sub-row at all and keeps
 * the client's own words.
 */
export function failedLine(row: Row): string | null {
  // A failed turn states itself: the row carries no reason text for it -
  // the failure's own words are in the seat's conversation - so the line
  // is the word alone.
  if (row.state.kind === 'failed-turn') return 'a turn failed';
  if (row.state.kind !== 'lifecycle') return null;
  const lifecycle = row.state.lifecycle;
  if (lifecycle !== 'Failed' && lifecycle !== 'AuthRequired') return null;
  return row.reason ?? (lifecycle === 'Failed' ? 'spawn failed' : 'not running');
}

/**
 * The reason line: what a person has to do about this project, in the row's
 * own words rather than a second vocabulary for the same two asks.
 *
 * A project whose worker is held reads as held, whether or not its lead is the
 * one held, so the workers are searched beside the lead. A failure is not
 * shared that way: it stays on the seat that failed, drawn on that seat's own
 * row ({@link failedLine}), so the project's line is the lead's alone.
 */
function whyOf(lead: Row, workers: Row[]): { line: string; bad: boolean } | null {
  for (const row of [lead, ...workers]) {
    if (row.pending !== null) {
      return {
        line:
          row.pending === 'question' ? 'asked you a question' : 'a permission prompt is waiting',
        bad: false,
      };
    }
  }
  const failed = failedLine(lead);
  return failed === null ? null : { line: failed, bad: true };
}

/**
 * The rail, grouped by the strongest state among each project's own rows.
 *
 * The projects keep the roster's order inside a group, because that is
 * `forge.toml`'s order and the rail is how a person reads the fleet - which is
 * why the rows come from `projectRows` rather than from the home's own
 * org-grouped view. The states themselves are the home's: a rail that derived
 * one for itself would disagree with the page a click away.
 *
 * **`closing` is the one state the caller brings in**, because it is the
 * client's own and not the home's: a seat this client has closed reads as
 * asleep the moment it is closed, so the reader's click moves it out of their
 * working section at once rather than leaving it sitting there for the
 * seconds the core takes to shut it down (#1712). The default answers false,
 * which is every caller but the rail.
 */
export function railGroups(
  home: HomeWire,
  current: SessionSlot,
  now: number,
  closing: (slot: SessionSlot) => boolean = () => false,
): RailGroup[] {
  const groups: RailGroup[] = [
    {
      heading: 'needs you',
      klass: 'state needs',
      hidden: null,
      holds: false,
      mark: null,
      projects: [],
    },
    { heading: 'working', klass: 'state', hidden: null, holds: false, mark: null, projects: [] },
    // The sleeping half of the fleet is the one nobody is working in, so it is
    // the one heading that folds: everything under it is still counted on the
    // heading, because a fold that reads as an empty section is worse than no
    // fold at all.
    { heading: 'asleep', klass: 'state', hidden: 0, holds: false, mark: null, projects: [] },
  ];

  for (const entry of home.projects) {
    const { lead, workers } = projectRows(home, entry);
    const all = [lead, ...workers];
    const rankOfRow = (row: Row): number =>
      closing(row.slot) ? 2 : rankOf(row.state, row.pending);
    const rank = all.reduce((best, row) => Math.min(best, rankOfRow(row)), 2);
    const group = groups[rank];
    if (group === undefined) continue;
    const sleeping = workers.filter((row) => rankOfRow(row) === 2);
    const shown =
      entry.project.org === current.org && entry.project.name === current.project
        ? current.label
        : null;
    if (shown !== null) group.holds = true;
    group.projects.push({
      name: entry.project.name,
      org: entry.project.org,
      shown,
      age: whenOf(lead, now),
      asleep: rank === 2,
      row: lead,
      workers: workers.filter((row) => rankOfRow(row) !== 2),
      sleeping,
      // A closing seat writes on no line: the lead's whole line goes with it,
      // and a closing worker is out of both searches.
      why: closing(lead.slot)
        ? null
        : whyOf(
            lead,
            workers.filter((row) => !closing(row.slot)),
          ),
    });
    // The rows the heading hides when it folds: the project's own row and
    // every worker under it, drawn or folded.
    if (group.hidden !== null) {
      group.hidden += 1 + workers.length;
      group.mark = foldMark([lead, ...workers].map((row) => rowMark(row, closing))) ?? group.mark;
    }
  }
  return groups.filter((group) => group.projects.length > 0);
}

/**
 * The seat's tree as the strip's row draws it: the branch and what moved on
 * the toggle, the pull request and the reason the tree could not be read in
 * the list.
 *
 * The files a diff holds are not here: the socket carries the tree's STATE -
 * the branch and the count - and the heavier scan that lists files and counts
 * their lines is not on this record, so the row states what the record holds
 * rather than drawing an empty list under it.
 */
export function gitStrip(record: SessionRecord, slot: SessionSlot): GitStrip | null {
  const { branch } = placeOf(record.work);
  const gate = gateLine(record.work.gate);
  const view = record.git;
  const uncommitted = view.worktree;
  const changed = record.work.changed;
  // The toggle reads the tree at a glance: where the branch is, how many
  // commits it runs ahead, the PR if it is on one - and `dirty` only when
  // the tree is, an absence being the clean word.
  const label = [
    branch,
    view.ahead !== null && view.ahead.count > 0
      ? `${view.ahead.count} commit${view.ahead.count === 1 ? '' : 's'}`
      : null,
    record.pr !== null ? `PR #${record.pr.number}` : null,
    changed !== null && changed > 0 ? 'dirty' : null,
  ]
    .filter((part) => part !== null)
    .join(' \u{b7} ');
  const clean = uncommitted === null && view.ahead === null && record.pr === null;
  // The default arrives as its remote-tracking ref (`origin/main`); compare
  // the plain name, the way the terminal's own row does, so a checked-out
  // `main` in a clone is recognised as the default rather than as work.
  const base = view.defaultBranch?.replace(/^origin\//, '') ?? null;
  const onDefault = gate === null && branch !== null && base !== null && branch === base;
  // The default branch with nothing on it has no state a row could state:
  // it is where work lands, not work. A dirty default branch still draws -
  // there is something to see.
  if (onDefault && clean) return null;
  return {
    label: label === '' ? 'no branch' : label,
    head: headOf(record.state.scan_cwd, slot),
    ahead:
      view.ahead === null
        ? null
        : {
            count: view.ahead.count,
            base: view.defaultBranch,
            commits: view.ahead.commits,
            stats: view.ahead.stats,
          },
    uncommitted,
    pr:
      record.pr === null
        ? null
        : {
            number: record.pr.number,
            url: record.pr.url,
            draft: record.pr.draft,
            closes: record.closes.map((issue) => `#${issue.number}`).join(' '),
          },
    gate,
  };
}

/**
 * What the tree IS: a worker's worktree names itself off the path the
 * worktrees live under, and everything else is the seat's own tree - the
 * project's when the seat is the lead, a worker's otherwise.
 */
function headOf(scanCwd: string, slot: SessionSlot): string {
  const marker = '/.claude/worktrees/';
  const at = scanCwd.indexOf(marker);
  if (at !== -1) {
    const name = scanCwd.slice(at + marker.length).replace(/\/+$/, '');
    return name === '' ? 'a worktree' : `worktree \u{b7} ${name}`;
  }
  return slot.label === 'lead' ? "the project's tree" : "a worker's tree";
}

/** The project this seat belongs to on the home's snapshot. */
export function projectOf(home: HomeWire, slot: SessionSlot): ProjectWire | null {
  return (
    home.projects.find(
      (row) => row.project.org === slot.org && row.project.name === slot.project,
    ) ?? null
  );
}

/** One usage window as its bar draws it. */
export interface AccountWindow {
  label: string;
  /** Clamped to 0..100 for the bar, which is what the terminal draws too. */
  percent: number;
  /**
   * The figure the row states, which is the UNCLAMPED one: an account past its
   * cap is the case a reader has to see, and a bar that stops at full plus a
   * label that says `101%` is what the server draws for it.
   */
  text: string;
  reset: string;
}

/** The account this seat's project binds to, and what the poller knows of it. */
export interface AccountView {
  name: string;
  /** The class the health dot carries: `ok` / `wait` / `bad`. */
  tone: string;
  windows: AccountWindow[];
  spend: { daily: string; weekly: string; monthly: string } | null;
  balance: string | null;
  /** The key's spending cap, or `null` when it declares none. */
  cap: string | null;
  /**
   * Whether the account bills per token, which is what decides whether its
   * figures are money or windows: `UsageSourceKind::OpenRouterKey` is the one
   * source that fills `spend`.
   */
  spendBilled: boolean;
}

/**
 * The account a spawn in this seat's project would bind to, with the pool's
 * own read of it.
 *
 * The name comes from the project's chip rather than from a second walk of the
 * pool: `chip_for` is the roster's answer to exactly this question, and two
 * answers would disagree the first time an account was pinned.
 */
export function accountChip(home: HomeWire, slot: SessionSlot): AccountView | null {
  const name = projectOf(home, slot)?.chip?.account_name ?? null;
  if (name === null) return null;

  const row = home.accounts.loading.find((entry) => entry.display_name === name);
  const usage = home.accounts.usage.find((entry) => entry.display_name === name);
  const snapshot = isRecord(usage?.snapshot) ? usage.snapshot : null;
  const windows: AccountWindow[] = [];
  for (const [label, key] of [
    ['5h', 'five_hour'],
    ['7d', 'seven_day'],
  ] as const) {
    const held = snapshot?.[key];
    if (!isRecord(held)) continue;
    const window = held;
    const utilization = typeof window['utilization'] === 'number' ? window['utilization'] : null;
    if (utilization === null) continue;
    windows.push({
      label,
      percent: Math.min(100, Math.max(0, utilization)),
      text: `${Math.round(utilization)}%`,
      reset: typeof window['reset_description'] === 'string' ? window['reset_description'] : '',
    });
  }

  const billed = snapshot?.['spend'];
  const spend = isRecord(billed) ? billed : null;
  const balance = snapshot?.['balance'];
  return {
    name,
    tone: row === undefined ? 'wait' : stateTone(row.state),
    windows,
    spend:
      spend === null
        ? null
        : {
            daily: money(spend['daily']),
            weekly: money(spend['weekly']),
            monthly: money(spend['monthly']),
          },
    balance: typeof balance === 'number' ? money(balance) : null,
    cap: spend !== null && typeof spend['limit'] === 'number' ? money(spend['limit']) : null,
    spendBilled: snapshot?.['source'] === 'OpenRouterKey',
  };
}

/** One figure on the rail's footer: what it is called and what it reads. */
export interface FooterFigure {
  label: string;
  value: string;
  /**
   * Whether the value is a placeholder rather than a reading, which is what
   * the cap row is whenever there is no amount to state.
   */
  dim: boolean;
}

/** The rail's footer: the account, what it is costing, and the two versions. */
export interface RailFooter {
  /** The account this project binds to, or `null` when none would serve it. */
  account: { name: string; tone: string } | null;
  /** The figures a spend-billed account reports; empty for a window-billed one. */
  figures: FooterFigure[];
  /** A window-billed account's windows, in the shape the chip already draws. */
  windows: AccountWindow[];
  /**
   * The pair the footer draws, side by side: what the server is and what it
   * speaks, beside what this client is and what it speaks. A mismatch is
   * then a difference the reader sees rather than a notice they have to
   * decode - and `skewed` is that difference, stated once.
   */
  versions: {
    serverForge: string;
    serverProtocol: number | null;
    clientForge: string;
    clientProtocol: number;
    skewed: boolean;
    claude: string | null;
    update: string | null;
  };
}

/**
 * What a figure nobody has reported reads.
 *
 * `$-` rather than `$0.00`, because a zero is a reading and forge has none -
 * the terminal's own placeholder, and the reason a cold account does not read
 * as one that has spent nothing.
 */
const UNPROBED = '$-';

/**
 * The rail's footer: the account serving this seat's project, what it is
 * costing, and the two builds behind the page.
 *
 * **The figures branch on the account's billing kind, which is the terminal's
 * own rule**: a spend-billed key gets its periods, balance and cap, and a
 * window-billed one gets its 5h and 7d windows in the same block. Drawn from
 * one snapshot either way, so the footer states what the poller reported
 * rather than a shape the account cannot fill.
 *
 * The account is read from the project's chip rather than from the seat's own
 * binding, which the client has no read for: the chip is the roster's answer
 * to which account serves this project, and the terminal names the seat's
 * bound account instead.
 */
export function railFooter(
  home: HomeWire,
  slot: SessionSlot,
  /** The greeting's protocol, or `null` before one lands. The rail always
   *  passes the live number; a test that does not care may leave it out. */
  serverProtocol: number | null = null,
): RailFooter {
  const versions = {
    serverForge: home.forge_version_short,
    // The greeting's own number beside this client's, because the two are
    // what a mismatch IS: `socket.ts` records them beside each other, and
    // the footer is where a reader sees both before one has to say so.
    serverProtocol,
    clientForge: CLIENT_VERSION,
    clientProtocol: PROTOCOL_VERSION,
    skewed: serverProtocol !== null && serverProtocol !== PROTOCOL_VERSION,
    claude: home.cli_version?.installed ?? null,
    update: availableVersion(home.cli_version?.installed ?? null, home.cli_version?.latest ?? null),
  };
  const account = accountChip(home, slot);
  if (account === null) return { account: null, figures: [], windows: [], versions };
  const head = { name: account.name, tone: account.tone };
  if (!account.spendBilled) {
    return { account: head, figures: [], windows: account.windows, versions };
  }
  const spend = account.spend;
  // Three states for the cap, as the terminal draws them: a cap to fill, a key
  // with none, and a snapshot that has not landed. The two placeholders are
  // dim, because a word saying there is no denominator is not a figure.
  const cap = account.cap ?? (spend === null ? '\u{2014}' : 'not set');
  return {
    account: head,
    figures: [
      { label: 'day', value: spend?.daily ?? UNPROBED, dim: false },
      { label: 'week', value: spend?.weekly ?? UNPROBED, dim: false },
      { label: 'month', value: spend?.monthly ?? UNPROBED, dim: false },
      { label: 'balance', value: account.balance ?? UNPROBED, dim: false },
      { label: 'cap', value: cap, dim: account.cap === null },
    ],
    windows: [],
    versions,
  };
}

function stateTone(state: string): string {
  if (state === 'ready') return 'ok';
  return state === 'bailed' ? 'bad' : 'wait';
}

function money(amount: unknown): string {
  return typeof amount === 'number' ? `$${amount.toFixed(2)}` : '';
}

/**
 * The project's tasks, as the strip's row draws them: in the order a person
 * reads them rather than the order the store returns them.
 */
export function taskRows(tasks: Task[], slot: SessionSlot): TaskStripRow[] {
  // The seat's own slice, scoped the way the terminal scopes its TASKS
  // section: a lead takes the top-level rows - its campaign board - and a
  // worker only the rows it owns. The whole set stays the home's read.
  const mine =
    slot.label === 'lead'
      ? tasks.filter((task) => task.parent === null)
      : tasks.filter((task) => task.owner?.label === slot.label);
  const rank: Record<string, number> = { in_progress: 0, blocked: 1, pending: 2, completed: 3 };
  const ordered = [...mine].sort((a, b) => (rank[a.status] ?? 9) - (rank[b.status] ?? 9));
  return ordered.map((task) => ({
    id: task.id,
    status: task.status,
    // A running row leads with its active form, the terminal's own rule: an
    // empty one falls back to the subject rather than drawing bare.
    display:
      task.status === 'in_progress' && task.active_form !== null && task.active_form !== ''
        ? task.active_form
        : task.subject,
    subject: task.subject,
    owner: task.owner === null ? null : task.owner.label,
    meta: taskMeta(task),
  }));
}

/**
 * A task's facts besides its owner: how far along it is, what it produced, and
 * how long it was thought to take.
 */
function taskMeta(task: Task): string {
  const parts = [chipFor(task.status)];
  if (task.artifact !== null) parts.push(artifactLabel(task.artifact));
  if (task.estimate !== null) parts.push(task.estimate);
  return parts.join(' \u{b7} ');
}

/** What a schedule is called: its own description, else the prompt's first line. */
function cronLabel(cron: CronEntry): string {
  if (cron.description !== undefined && cron.description !== '') return cron.description;
  return cron.prompt.split('\n')[0] ?? '';
}

/** One schedule, as the strip's row draws it. */
export interface SeatScheduleRow {
  /** The cron's own id: what the row is KEYED by, since two schedules can
   *  share a description, or a first prompt line, and a row keyed by the
   *  drawn words throws on exactly that pair. */
  id: string;
  /** The line the row leads with: the description, else the prompt's first line. */
  key: string;
  /** What the row says beside it: when it is next due, and its kind. */
  value: string;
}

/**
 * The project's schedules, as the seat's strip row draws them.
 *
 * **Ownership is `team_role`**: a cron names the worker label it was created
 * by, and `None` targets the project lead - so the lead's page reads the None
 * set and a worker's page reads its own label's, the same rule the connector
 * row beside it applies. The countdown reads against the page's one clock, so
 * the caller re-reads as that clock moves.
 */
export function seatScheduleRows(
  home: HomeWire,
  slot: SessionSlot,
  now: number,
): SeatScheduleRow[] {
  const crons = projectOf(home, slot)?.crons ?? [];
  const mine = crons.filter((cron) =>
    slot.label === 'lead' ? (cron.team_role ?? null) === null : cron.team_role === slot.label,
  );
  return mine.map((cron) => ({
    id: cron.id,
    key: cronLabel(cron),
    value: `${untilOf(cron.next_fire, now)} \u{b7} ${kindOf(cron.kind)}`,
  }));
}

/** `CronKind` is externally tagged, so its variant name is the key. */
function kindOf(kind: unknown): string {
  const held = isRecord(kind) ? kind : {};
  return 'Once' in held ? 'one-shot' : 'recurring';
}

/**
 * How long until `at`. A time the clock has already passed is due rather than
 * a countdown into the past.
 */
export function untilOf(at: { secs_since_epoch: number } | null, now: number): string {
  if (at === null) return 'due now';
  const remaining = at.secs_since_epoch - Math.floor(now / 1000);
  if (remaining <= 0) return 'due now';
  if (remaining <= 59) return 'in a minute';
  if (remaining < 3600) return `in ${Math.floor(remaining / 60)}m`;
  if (remaining < 86_400) return `in ${Math.floor(remaining / 3600)}h`;
  return `in ${Math.floor(remaining / 86_400)}d`;
}

/**
 * What a subscription watches: a conversation by its name, or the class it
 * covers.
 *
 * **Two of the three arms cross as BARE STRINGS, not objects**, because
 * `SlackSubscriptionTarget` is a serde enum whose unit variants are exactly
 * that on the wire - only `Conversation` is an object. Reading all three as
 * objects misses the two class arms, and the row then draws an empty key.
 */
function targetOf(target: unknown): string {
  if (target === 'DirectMessages') return 'direct messages';
  if (target === 'Mentions') return 'mentions anywhere';
  const conversation = conversationOf(target);
  const name = conversation['name'];
  if (typeof name === 'string' && name !== '') return name;
  return typeof conversation['id'] === 'string' ? conversation['id'] : '';
}

/** What a subscription lets through. */
function modeOf(target: unknown): string {
  if (target === 'DirectMessages') return 'every message';
  if (target === 'Mentions') return 'mentions only';
  return conversationOf(target)['mode'] === 'MentionsOnly' ? 'mentions only' : 'every message';
}

/** The `Conversation` arm's fields, or an empty bag for the two class arms. */
function conversationOf(target: unknown): Record<string, unknown> {
  const held = isRecord(target) ? target['Conversation'] : null;
  return isRecord(held) ? held : {};
}

/** One project's connector subscriptions, as the strip's rows draw them. */
interface RowConnectorViews {
  gotify: {
    id: string;
    applications: string[];
    min_priority: number | null;
    team_role: string | null;
  }[];
  slack: { id: string; workspace: string; target: unknown; team_role: string | null }[];
}

/**
 * One seat's own connector subscriptions, as a row draws them.
 *
 * **Ownership is `team_role`**: a subscription names the worker label it
 * belongs to, and `None` targets the project lead - so the lead's page reads
 * the None set and a worker's page reads its own label's, and neither shows
 * the other's. The sets ride the project's home row (all of the project's,
 * both owners in one list), which is why the seat filter happens here.
 */
export interface SeatConnectorRow {
  /** Which connector the row belongs to: its glyph names it at a glance. */
  kind: 'gotify' | 'slack';
  /** The subscription's own id, which is what a row is KEYED by: two
   *  subscriptions in one workspace can draw the same words (a mentions
   *  watcher beside the auto-subscribed conversation), and keying by the
   *  drawn text throws on exactly that pair. */
  id: string;
  /** The line the row leads with: the apps, or what the slack target watches. */
  key: string;
  /** What the row says beside it: the floor, or the slack mode - and the
   *  connector's own state where the stream is not up, because a
   *  disconnected stream must not read as if all were well. */
  value: string;
}

export function seatConnectorRows(home: HomeWire, slot: SessionSlot): SeatConnectorRow[] {
  const held = rowConnectors(projectOf(home, slot));
  const live = connectorsLiveness(home);
  const mine = <T extends { team_role: string | null }>(subs: T[]): T[] =>
    subs.filter((sub) =>
      slot.label === 'lead' ? sub.team_role === null : sub.team_role === slot.label,
    );

  const rows: SeatConnectorRow[] = [];
  for (const sub of mine(held.gotify)) {
    const parts = [sub.min_priority === null ? 'any priority' : `>=${sub.min_priority}`];
    if (!live.gotify) parts.push('offline');
    rows.push({
      kind: 'gotify',
      id: sub.id,
      key: sub.applications.length === 0 ? 'any app' : sub.applications.join(', '),
      value: parts.join(' \u{b7} '),
    });
  }
  for (const sub of mine(held.slack)) {
    const parts = [targetOf(sub.target), modeOf(sub.target)];
    if (!live.slack.has(sub.workspace)) parts.push('not connected');
    rows.push({
      kind: 'slack',
      id: sub.id,
      key: sub.workspace,
      value: parts.join(' \u{b7} '),
    });
  }
  // A boot that could not read the durable slack subscriptions leaves the
  // project's set empty, so the failure draws as a row of its own - this is
  // the only surface left that can say so (rule 22's failed state).
  if (live.slackFailed) {
    rows.push({
      kind: 'slack',
      id: 'slack-load',
      key: 'slack',
      value: 'subscriptions failed to load',
    });
  }
  return rows;
}

/**
 * The connectors' liveness, as the rows state it: whether the gotify stream
 * is up, and the slack workspaces that are connected. A subscription row says
 * so where its stream is not up - the one fact that must not go unrendered
 * (rule 22), since a silent row reads as a working one.
 */
function connectorsLiveness(home: HomeWire): {
  gotify: boolean;
  slack: Set<string>;
  slackFailed: boolean;
} {
  const held = isRecord(home.connectors) ? home.connectors : {};
  const gotify = isRecord(held['gotify']) ? held['gotify'] : {};
  const slack = isRecord(held['slack']) ? held['slack'] : {};
  const connected = new Set<string>();
  for (const entry of array(slack['connected_workspaces'])) {
    if (Array.isArray(entry) && entry[1] === true && typeof entry[0] === 'string') {
      connected.add(entry[0]);
    }
  }
  return {
    gotify: gotify['connected'] === true,
    slack: connected,
    slackFailed: slack['load_failed'] === true,
  };
}

/**
 * One project's subscriptions, narrowed where they enter.
 *
 * A seat with no row on the home - a project that left `forge.toml` between
 * the record and this snapshot - reads as nothing subscribed rather than as
 * the page's problem: the strip's row draws nothing, the way it does for a
 * project nobody has subscribed anything to.
 */
function rowConnectors(project: ProjectWire | null): RowConnectorViews {
  const held = isRecord(project?.connectors) ? project.connectors : {};
  return {
    gotify: array(held['gotify']).map((entry) => {
      const sub = isRecord(entry) ? entry : {};
      const id = sub['id'];
      return {
        id: typeof id === 'string' ? id : '',
        applications: array(sub['applications']).map((app) => String(app)),
        min_priority: typeof sub['min_priority'] === 'number' ? sub['min_priority'] : null,
        team_role: typeof sub['team_role'] === 'string' ? sub['team_role'] : null,
      };
    }),
    slack: array(held['slack']).map((entry) => {
      const sub = isRecord(entry) ? entry : {};
      const workspace = sub['workspace'];
      const id = sub['id'];
      return {
        id: typeof id === 'string' ? id : '',
        workspace: typeof workspace === 'string' ? workspace : '',
        target: sub['target'],
        team_role: typeof sub['team_role'] === 'string' ? sub['team_role'] : null,
      };
    }),
  };
}

/**
 * The session's MCP servers, as the strip's row draws them: one row per
 * server, carrying the status depth (its tools, what backs it, the failure
 * reason) so the panel shows as much of a server as the wire has.
 *
 * A read that FAILED carries an empty list - and that is a state of its own,
 * not "nothing configured" - so the failure draws as a row naming it.
 */
export function mcpRows(record: SessionRecord | null): McpRow[] {
  const servers = record?.mcp ?? null;
  if (servers === null) return [];
  if (servers.servers.length === 0) {
    return servers.error === null
      ? []
      : [
          {
            name: 'mcp-read',
            k: 'servers',
            v: 'failed',
            tools: [],
            command: null,
            reason: servers.error.trim() === '' ? null : servers.error.trim(),
            synthetic: true,
          },
        ];
  }
  return servers.servers.map((server) => ({
    name: server.name,
    k: `${server.name} \u{b7} ${scopeLabel(server)}`,
    v: mcpState(server),
    tools: mcpTools(server),
    command: mcpCommand(server),
    reason: server.error?.trim() ? server.error.trim() : null,
    synthetic: false,
  }));
}

/** The tools a server offers, as its status depth names them. */
function mcpTools(server: McpServer): string[] {
  return array(server.tools).flatMap((entry) => {
    const held = isRecord(entry) ? entry : {};
    const name = held['name'];
    return typeof name === 'string' && name !== '' ? [name] : [];
  });
}

/**
 * What backs a server, from its config blob: the command a stdio server runs
 * (its argv joined), or the URL a remote one reaches. `null` for a config
 * that names neither, which is the in-process case.
 */
function mcpCommand(server: McpServer): string | null {
  if (!isRecord(server.config)) return null;
  const url = server.config['url'];
  if (typeof url === 'string' && url !== '') return url;
  const command = server.config['command'];
  if (typeof command !== 'string' || command === '') return null;
  const args = array(server.config['args']).map((arg) => String(arg));
  return [command, ...args].join(' ');
}

/**
 * The scope a server is configured in, which is what its config blob names
 * rather than where its process runs. A server that reports no scope reads as
 * the session's.
 */
function scopeLabel(server: McpServer): string {
  if (server.scope !== undefined) return server.scope;
  return isRecord(server.config) && server.config['type'] === 'sdk' ? 'sdk' : 'session';
}

/**
 * What a row says in its value column: how many tools the server offers when
 * it is up, and why it is not when it is not.
 */
export function mcpState(server: McpServer): string {
  switch (server.status) {
    case 'connected':
      return server.tools === undefined ? 'connected' : toolSummary(server.tools.length);
    case 'failed':
      // The reason is the row's own line beneath, not this cell: the terminal
      // draws it once, and a cell carrying it again would say it twice.
      return 'failed';
    case 'needs-auth':
      return 'needs sign-in';
    case 'pending':
      return 'connecting';
    case 'disabled':
      return 'disabled';
  }
}

/** How many tools a server offers, counted so that one reads as one. */
export function toolSummary(count: number): string {
  if (count === 0) return 'no tools';
  return count === 1 ? '1 tool' : `${count} tools`;
}

/** Resident memory, in the unit the reader thinks in. */
export function memoryLabel(bytes: number): string {
  const KB = 1024;
  const MB = 1024 * KB;
  const GB = 1024 * MB;
  if (bytes < KB) return `${bytes} B`;
  if (bytes < MB) return `${Math.floor(bytes / KB)} KB`;
  if (bytes < GB) return `${Math.floor(bytes / MB)} MB`;
  return `${Math.floor(bytes / GB)}.${Math.floor((bytes % GB) / (GB / 10))} GB`;
}

/** The session's monitors, as the strip's row draws them. */
export function monitorRows(monitors: MonitorRecord[], now: number): MonitorStripRow[] {
  return monitors.map((monitor) => ({
    id: monitor.tool_use_id,
    running: monitor.status === 'running',
    // Only `completed` is a success: the wire folds failed, killed and
    // stopped into `stopped`, so anything else must not wear the green
    // check - the terminal's own row draws them red for the same reason.
    completed: monitor.status === 'completed',
    name: monitor.description,
    label: monitorLabel(monitor, now),
    command: monitor.command,
  }));
}

/**
 * The trailing word on a monitor's own row: how it ended, or what it is while
 * it runs.
 *
 * A settled row carries the age of its end beside it, from the instant the
 * wire stamped on the transition. Only when the record holds one: a transition
 * that stated no instant draws the word alone rather than an age counted from
 * the status, which would be a number nothing said.
 */
export function monitorLabel(monitor: MonitorRecord, now: number): string {
  const word =
    monitor.status === 'running'
      ? monitor.persistent
        ? 'persistent'
        : 'running'
      : monitor.status === 'completed'
        ? 'completed'
        : monitor.status === 'stopped'
          ? 'stopped'
          : 'timed out';
  if (monitor.status !== 'running' && monitor.ended_at !== null) {
    return `${word} ${elapsedLabel(monitor.ended_at, now)}`;
  }
  return word;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function array(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

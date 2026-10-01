/**
 * The session page's reads gathered into the shape its markup wants - the
 * client half of the gather `crates/forge-web/src/session.rs` does across its
 * own render functions.
 *
 * Pure, so a test can build a session by hand. Nothing here recomputes a state
 * the server already decided: a row's lifecycle, a monitor's status and a
 * server's connection state all arrive in the record, and a section is drawn
 * only when the record has something behind it - a section that is always
 * there says nothing when it is empty.
 *
 * Four of the nine sections do not read the seat's own record. A project's
 * tasks, its schedules and the connector views are keyed by PROJECT on the
 * home's snapshot, and the home is the subscription the shell already holds
 * for the rest of the app.
 */

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
import type { Connection } from '../socket';
import type { CronEntry, HomeWire, Lifecycle, ProjectWire, Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import type {
  McpServer,
  MonitorRecord,
  ProcessEntry,
  ProcessSnapshot,
  SessionHeader,
  SessionRecord,
} from './wire';

/** The facts the header states, and the class the mode's chip carries. */
export interface Facts {
  /** The occupant's id, or `null` on a seat nothing has started. */
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

/** The git section's rows. */
export interface GitView {
  summary: string;
  /** `PR #N`, when the branch is on one. */
  pr: number | null;
  /** What that pull request closes, as one line. */
  closes: string | null;
  /** Why there is no branch to show, when the tree could not be read. */
  gate: string | null;
  /**
   * Whether the section opens on first draw.
   *
   * The section itself is drawn for every seat with a working tree, the way
   * the server draws it: a branch and a count are worth stating even with
   * nothing under them, and the one read whose absence would read as "this
   * seat has no repository" is the whole section, not its body. It opens only
   * when there is a body to open on, or a clean tree on no pull request would
   * lead the inspector with an open section and nothing under it.
   */
  open: boolean;
}

/** One row of a section body: a key, a value, and how the value is weighted. */
export interface Kv {
  k: string;
  v: string;
  /** The value carries the accent: what a row means rather than what it is. */
  accent?: boolean;
}

/** One task as its row draws it. */
export interface TaskRow {
  klass: string;
  subject: string;
  owner: string | null;
  meta: string;
}

/** One schedule as its row draws it. */
export interface ScheduleRow {
  k: string;
  v: string;
}

/** A slack workspace, with the subscriptions that watch it under it. */
export interface SlackWorkspace {
  name: string;
  connected: boolean;
  /** `id` is the wire's own, and it is what a row is keyed by: two
   * subscriptions in one workspace can draw the same words. */
  subs: { id: string; k: string; v: string }[];
}

/** The gotify section, as its rows draw it. */
export interface GotifyView {
  summary: string;
  rows: Kv[];
}

/** The slack section, as its rows draw it. */
export interface SlackView {
  summary: string;
  workspaces: SlackWorkspace[];
}

/** The MCP section, as its rows draw it. */
export interface McpView {
  summary: string;
  rows: Kv[];
  /** Why the read failed, when it did: an empty list alone cannot say. */
  error: string | null;
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
  cwd: string;
  /** Whether a seat is behind this page at all. */
  waking: boolean;
  /** Why it is not running, when the roster says. */
  reason: string | null;
  /** A compaction in flight, which the conversation draws a line for. */
  compacting: boolean;
  slot: SessionSlot;
  connection: Connection;
}

/** What the composer is handed, and what its own task writes against. */
export interface ComposerProps {
  record: SessionRecord;
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

/**
 * One process as its row draws it, with the processes below it.
 *
 * The shape is the tree rather than a flat list with a depth on each row: the
 * depth IS the nesting, so a row does not have to carry a number that says
 * what its position already says.
 */
export interface ProcessNode {
  headline: string;
  memory: string;
  pid: number;
  children: ProcessNode[];
}

/** One monitor as its card draws it. */
export interface MonitorView {
  running: boolean;
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
 * What the copy control's click did, or what stands in the way of one.
 *
 * `no-clipboard` and `failed` are the two failures kept apart because they are
 * different problems for the reader: the first is the page's origin, the
 * second is a write the OS refused.
 */
export type CopyOutcome = 'ready' | 'copied' | 'failed' | 'no-clipboard';

/** What the copy control says on the row. */
export function copyLabel(outcome: CopyOutcome): string {
  switch (outcome) {
    case 'ready':
      return 'copy';
    case 'copied':
      return 'copied';
    case 'failed':
      return 'copy failed';
    case 'no-clipboard':
      return 'copy needs https';
  }
}

/**
 * What the control is for: its accessible name, and the reason a state other
 * than `copy` is showing.
 *
 * The name is spelt out rather than left as the visible word, so a reader who
 * cannot see the id beside it still knows what the click does.
 */
export function copyReason(outcome: CopyOutcome): string {
  switch (outcome) {
    case 'ready':
      return 'copy the whole session id';
    case 'copied':
      return 'the whole session id is on the clipboard';
    case 'failed':
      return 'the clipboard refused the write';
    case 'no-clipboard':
      return 'this page has no clipboard to write to: it needs a secure origin';
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
 * The reason line: what a person has to do about this project, in the row's
 * own words rather than a second vocabulary for the same two asks.
 *
 * A project whose worker is held reads as held, whether or not its lead is the
 * one held, so the workers are searched beside the lead.
 */
function whyOf(rows: Row[]): { line: string; bad: boolean } | null {
  for (const row of rows) {
    if (row.pending !== null) {
      return {
        line:
          row.pending === 'question' ? 'asked you a question' : 'a permission prompt is waiting',
        bad: false,
      };
    }
  }
  for (const row of rows) {
    if (row.state.kind !== 'lifecycle') continue;
    if (row.state.lifecycle !== 'Failed' && row.state.lifecycle !== 'AuthRequired') continue;
    return { line: row.reason ?? 'not running', bad: true };
  }
  return null;
}

/**
 * The rail, grouped by the strongest state among each project's own rows.
 *
 * The projects keep the roster's order inside a group, because that is
 * `forge.toml`'s order and the rail is how a person reads the fleet - which is
 * why the rows come from `projectRows` rather than from the home's own
 * org-grouped view. The states themselves are the home's: a rail that derived
 * one for itself would disagree with the page a click away.
 */
export function railGroups(home: HomeWire, current: SessionSlot, now: number): RailGroup[] {
  const groups: RailGroup[] = [
    { heading: 'needs you', klass: 'state needs', hidden: null, projects: [] },
    { heading: 'working', klass: 'state', hidden: null, projects: [] },
    // The sleeping half of the fleet is the one nobody is working in, so it is
    // the one heading that folds: everything under it is still counted on the
    // heading, because a fold that reads as an empty section is worse than no
    // fold at all.
    { heading: 'asleep', klass: 'state', hidden: 0, projects: [] },
  ];

  for (const entry of home.projects) {
    const { lead, workers } = projectRows(home, entry);
    const all = [lead, ...workers];
    const rank = all.reduce((best, row) => Math.min(best, rankOf(row.state, row.pending)), 2);
    const group = groups[rank];
    if (group === undefined) continue;
    const sleeping = workers.filter((row) => rankOf(row.state, row.pending) === 2);
    group.projects.push({
      name: entry.project.name,
      org: entry.project.org,
      shown:
        entry.project.org === current.org && entry.project.name === current.project
          ? current.label
          : null,
      age: whenOf(lead, now),
      asleep: rank === 2,
      row: lead,
      workers: workers.filter((row) => rankOf(row.state, row.pending) !== 2),
      sleeping,
      why: whyOf(all),
    });
    // The rows the heading hides when it folds: the project's own row and
    // every worker under it, drawn or folded.
    if (group.hidden !== null) group.hidden += 1 + workers.length;
  }
  return groups.filter((group) => group.projects.length > 0);
}

/**
 * The git section: the branch the seat's tree is on and what moved in it, the
 * pull request that tree belongs to, and why there is no branch when there is
 * not one.
 *
 * The files a diff holds are not here: the socket carries the tree's STATE -
 * the branch and the count - and the heavier scan that lists files and counts
 * their lines is not on this record, so the section states what the record
 * holds rather than drawing an empty list under it.
 */
export function gitSection(record: SessionRecord): GitView {
  const { branch, files } = placeOf(record.work);
  const gate = gateLine(record.work.gate);
  return {
    summary: [branch, files].filter((part) => part !== null).join(' \u{b7} '),
    pr: record.pr === null ? null : record.pr.number,
    closes:
      record.closes.length === 0
        ? null
        : record.closes.map((issue) => `#${issue.number}`).join(' '),
    gate,
    open: gate !== null || record.pr !== null || files !== null,
  };
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

/** The account chip: who a spawn here would bind to, and what the poller knows. */
export interface AccountView {
  name: string;
  /** `probing` / `ready` / `bailed`, or `null` when the pool has no row for it. */
  state: string | null;
  tone: string;
  /** Which repair instruction the account earns. */
  auth: string | null;
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
    state: row === undefined ? null : stateWord(row.state),
    tone: row === undefined ? 'wait' : stateTone(row.state),
    auth: row === undefined ? null : authWord(row.auth),
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
  versions: { forge: string; claude: string | null; update: string | null };
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
export function railFooter(home: HomeWire, slot: SessionSlot): RailFooter {
  const versions = {
    forge: home.forge_version_short,
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

function stateWord(state: string): string {
  if (state === 'ready') return 'ready';
  return state === 'bailed' ? 'bailed' : 'probing';
}

function stateTone(state: string): string {
  if (state === 'ready') return 'ok';
  return state === 'bailed' ? 'bad' : 'wait';
}

/** What a bailed account's repair instruction names. */
function authWord(auth: string): string {
  return auth === 'base_url' ? 'base url' : 'token';
}

function money(amount: unknown): string {
  return typeof amount === 'number' ? `$${amount.toFixed(2)}` : '';
}

/**
 * The tasks section: what the project holds, in the order a person reads them
 * rather than the order the store returns them.
 */
export function tasksSection(tasks: Task[]): { summary: string; rows: TaskRow[] } {
  const done = tasks.filter((task) => task.status === 'completed').length;
  const rank: Record<string, number> = { in_progress: 0, blocked: 1, pending: 2, completed: 3 };
  const ordered = [...tasks].sort((a, b) => (rank[a.status] ?? 9) - (rank[b.status] ?? 9));
  return {
    summary: `${done} of ${tasks.length}`,
    rows: ordered.map((task) => ({
      klass: taskClass(task.status),
      subject: task.subject,
      owner: task.owner === null ? null : task.owner.label,
      // The owner is drawn beside these, as their own cell: the terminal
      // splits them, and a `·`-joined string could not weight them apart.
      meta: taskMeta(task),
    })),
  };
}

function taskClass(status: string): string {
  if (status === 'in_progress') return 'tk now';
  if (status === 'completed') return 'tk done';
  if (status === 'blocked') return 'tk blocked';
  return 'tk';
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

/** The schedules section: the crons that fire into this project, and when. */
export function schedulesSection(
  crons: CronEntry[],
  now: number,
): { summary: string; rows: ScheduleRow[] } {
  return {
    summary: `${crons.length}`,
    rows: crons.map((cron) => ({
      k: cronLabel(cron),
      v: `${untilOf(cron.next_fire, now)} \u{b7} ${kindOf(cron.kind)}`,
    })),
  };
}

/** What a schedule is called: its own description, else the prompt's first line. */
function cronLabel(cron: CronEntry): string {
  if (cron.description !== undefined && cron.description !== '') return cron.description;
  return cron.prompt.split('\n')[0] ?? '';
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
  if (remaining <= 59) return 'in a minute';
  if (remaining < 3600) return `in ${Math.floor(remaining / 60)}m`;
  if (remaining < 86_400) return `in ${Math.floor(remaining / 3600)}h`;
  return `in ${Math.floor(remaining / 86_400)}d`;
}

/** The gotify section, or `null` when nothing is subscribed and it is not up. */
export function gotifySection(home: HomeWire): GotifyView | null {
  const view = connectorsOf(home).gotify;
  if (view === null) return null;
  if (!view.connected && view.subscriptions.length === 0) return null;

  const apps: string[] = [];
  for (const sub of view.subscriptions) {
    for (const app of sub.applications) if (!apps.includes(app)) apps.push(app);
  }
  // One subscription with no floor makes the whole set unbounded, which is
  // what `any` says: a delivery is let through when ANY subscription matches,
  // so the set's floor is the lowest of them and an absent one removes it.
  const floors = view.subscriptions.map((sub) => sub.min_priority);
  const floor = floors.includes(null)
    ? null
    : floors.reduce<number | null>(
        (lowest, value) =>
          value === null ? lowest : lowest === null ? value : Math.min(lowest, value),
        null,
      );
  return {
    summary: view.connected ? 'connected' : 'not connected',
    rows: [
      { k: 'apps', v: apps.join(', ') },
      { k: 'priority', v: floor === null ? 'any' : `>=${floor}` },
    ],
  };
}

/**
 * The slack section, or `null` when the home carries no workspace.
 *
 * A workspace and its subscriptions come back as a parent and its children
 * rather than as a flat list, because the hierarchy is what the section has to
 * draw: a subscription watches a workspace, and a target prefixed with two
 * spaces said so in a way nothing could style or wrap.
 */
export function slackSection(home: HomeWire): SlackView | null {
  const view = connectorsOf(home).slack;
  if (view === null) return null;
  const names: string[] = view.connected_workspaces.map(([name]) => name);
  for (const sub of view.subscriptions) {
    if (!names.includes(sub.workspace)) names.push(sub.workspace);
  }
  if (names.length === 0) return null;

  return {
    summary: `${names.length} workspace${names.length === 1 ? '' : 's'}`,
    workspaces: names.map((name) => ({
      name,
      connected: view.connected_workspaces.find(([held]) => held === name)?.[1] === true,
      subs: view.subscriptions
        .filter((sub) => sub.workspace === name)
        .map((sub) => ({ id: sub.id, k: targetOf(sub.target), v: modeOf(sub.target) })),
    })),
  };
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

/** The connector views, which the home's snapshot carries beside the fleet. */
interface ConnectorViews {
  gotify: {
    connected: boolean;
    subscriptions: { applications: string[]; min_priority: number | null }[];
  } | null;
  slack: {
    connected_workspaces: [string, boolean][];
    subscriptions: { id: string; workspace: string; target: unknown }[];
  } | null;
}

/**
 * The connectors, narrowed where they enter.
 *
 * The home's own type leaves this member `unknown`, because no page in that
 * slice drew one; the two section bodies above are the readers, so the
 * narrowing belongs here rather than in the wire's own module.
 */
function connectorsOf(home: HomeWire): ConnectorViews {
  const held = isRecord(home.connectors) ? home.connectors : {};
  const gotify = isRecord(held['gotify']) ? held['gotify'] : null;
  const slack = isRecord(held['slack']) ? held['slack'] : null;
  return {
    gotify:
      gotify === null
        ? null
        : {
            connected: gotify['connected'] === true,
            subscriptions: array(gotify['subscriptions']).map((entry) => {
              const sub = isRecord(entry) ? entry : {};
              return {
                applications: array(sub['applications']).map((app) => String(app)),
                min_priority: typeof sub['min_priority'] === 'number' ? sub['min_priority'] : null,
              };
            }),
          },
    slack:
      slack === null
        ? null
        : {
            connected_workspaces: array(slack['connected_workspaces']).flatMap((entry) =>
              Array.isArray(entry) && typeof entry[0] === 'string'
                ? [[entry[0], entry[1] === true] as [string, boolean]]
                : [],
            ),
            subscriptions: array(slack['subscriptions']).map((entry) => {
              const sub = isRecord(entry) ? entry : {};
              const workspace = sub['workspace'];
              const id = sub['id'];
              return {
                id: typeof id === 'string' ? id : '',
                workspace: typeof workspace === 'string' ? workspace : '',
                target: sub['target'],
              };
            }),
          },
  };
}

/** The MCP section, or `null` when the session has no read behind it. */
export function mcpSection(record: SessionRecord): McpView | null {
  const servers = record.mcp;
  if (servers === null) return null;
  if (servers.servers.length === 0 && servers.error === null) return null;
  return {
    // A read that failed carries an empty list, so the summary says which of
    // the two it is looking at rather than reporting nothing configured.
    summary: servers.servers.length === 0 ? 'failed' : `${servers.servers.length}`,
    rows: servers.servers.map((server) => ({
      k: `${server.name} \u{b7} ${scopeLabel(server)}`,
      v: mcpState(server),
    })),
    error: servers.error,
  };
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
    case 'failed': {
      const error = server.error?.trim() ?? '';
      return error === '' ? 'failed' : error;
    }
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

/**
 * The processes section as a tree: parents before their children, and the
 * walk's own order within each sibling group.
 *
 * **The nesting is the tree rather than an indent counted onto a flat row.**
 * The terminal wrote two `&nbsp;` per level inside the row's key cell, which
 * is a character run standing in for hierarchy: no rule could reach it, it did
 * not wrap, and a space is not a layout step. A list inside a list is the
 * shape the section was drawing, and it is the shape the home's own nested
 * rows already use.
 *
 * The walk returns entries by memory rather than by parentage, so drawing them
 * in that order would nest a row under whatever happened to come before it. A
 * row whose parent the walk did not carry is a root of its own, which is what
 * makes a partial snapshot still list everything in it.
 */
export function processTree(walk: ProcessSnapshot): ProcessNode[] {
  const childrenOf = new Map<number, ProcessEntry[]>();
  const present = new Set<number>();
  for (const entry of walk.processes) {
    const siblings = childrenOf.get(entry.parent_pid);
    if (siblings === undefined) childrenOf.set(entry.parent_pid, [entry]);
    else siblings.push(entry);
    present.add(entry.pid);
  }

  const placed = new Set<number>();
  const take = (entry: ProcessEntry): ProcessNode => {
    const children: ProcessNode[] = [];
    for (const child of childrenOf.get(entry.pid) ?? []) {
      if (placed.has(child.pid)) continue;
      placed.add(child.pid);
      children.push(take(child));
    }
    return {
      headline: processHeadline(entry),
      memory: memoryLabel(entry.memory_bytes),
      pid: entry.pid,
      children,
    };
  };

  const roots: ProcessNode[] = [];
  for (const entry of walk.processes) {
    if (present.has(entry.parent_pid) || placed.has(entry.pid)) continue;
    placed.add(entry.pid);
    roots.push(take(entry));
  }
  // A pid cycle reaches no root, and neither does a subtree hanging off one.
  // Every row is still drawn, once.
  for (const entry of walk.processes) {
    if (placed.has(entry.pid)) continue;
    placed.add(entry.pid);
    roots.push(take(entry));
  }
  return roots;
}

/**
 * What a row calls its process: the command it is running, with the
 * executable's path stripped, and the command a shell wrapper wraps rather
 * than its own chrome.
 */
export function processHeadline(entry: ProcessEntry): string {
  const command = entry.command.trim();
  const inner = extractInnerCommand(entry.command);
  if (inner !== null) return basenameExe(inner);
  if (command === '') return entry.name === '' ? '(process)' : entry.name;
  return basenameExe(command);
}

/**
 * The command a shell wrapper wraps, or `null` when this is not one.
 *
 * It terminates at the OUTERMOST `' < /dev/null`, so a command that itself
 * contains that redirect does not cut off early, and reverses the POSIX escape
 * the wrapper applies to a single quote.
 */
export function extractInnerCommand(cmdline: string): string | null {
  const afterEval = cmdline.split("eval '")[1];
  if (afterEval === undefined) return null;
  const at = afterEval.lastIndexOf("' < /dev/null");
  if (at < 0) return null;
  return afterEval.slice(0, at).trim().replaceAll(`'"'"'`, "'");
}

/** The executable's directory stripped from a headline, args kept verbatim. */
export function basenameExe(cmdline: string): string {
  const trimmed = cmdline.trim();
  const at = trimmed.search(/\s/);
  const cut = (value: string): string => value.split('/').pop() ?? value;
  if (at < 0) return cut(trimmed);
  return `${cut(trimmed.slice(0, at))} ${trimmed.slice(at + 1)}`;
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

/**
 * When the walk behind these rows was taken.
 *
 * The walk is only ever performed for the session a view is looking at, so a
 * slot nobody is looking at serves the last tree left on it, and rows from an
 * hour ago drawn exactly like rows from a second ago would be a wrong answer
 * rather than an old one.
 */
export function walkedNote(
  scannedAt: { secs_since_epoch: number; nanos_since_epoch: number },
  now: number,
): string {
  const age = elapsedLabel(scannedAt, now);
  return age === 'now' ? 'walked just now' : `walked ${age} ago`;
}

/** The monitors section, as its cards draw it. */
export function monitorsSection(
  monitors: MonitorRecord[],
  now: number,
): { summary: string; rows: MonitorView[] } {
  const running = monitors.filter((monitor) => monitor.status === 'running').length;
  return {
    summary: `${running} running`,
    rows: monitors.map((monitor) => ({
      running: monitor.status === 'running',
      name: monitor.description,
      label: monitorLabel(monitor, now),
      command: monitor.command,
    })),
  };
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

/**
 * Whether the conversation holds a sub-agent dispatch at all.
 *
 * **The folded instances are not on the socket, and this is all a view can
 * know.** `subagents` on the record is the catalogue of agent TYPES the CLI
 * offers, and a page of history carries `ChatUnit`, which has no instance
 * variant - so neither the snapshot nor the `more` answer holds the cards the
 * inspector's section draws. Until the fold crosses, the section says so when
 * the conversation shows a dispatch: an absent section would read as "no
 * sub-agents ran", which is the same mistake as drawing a settled state for
 * one nobody described.
 */
export function hasDispatches(messages: unknown[]): boolean {
  return messages.some((message) => {
    const frame = isRecord(message) ? message : {};
    if (frame['type'] !== 'assistant') return false;
    const parent = frame['parent_tool_use_id'];
    if (typeof parent === 'string' && parent.trim() !== '') return false;
    const inner = isRecord(frame['message']) ? frame['message'] : {};
    return array(inner['content']).some((block) => {
      const use = isRecord(block) ? block : {};
      return use['type'] === 'tool_use' && (use['name'] === 'Task' || use['name'] === 'Agent');
    });
  });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function array(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

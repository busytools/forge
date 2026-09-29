/**
 * The home's reads gathered into the shape its markup wants - the client half
 * of the gather `crates/forge-web/src/home.rs` does in `view_of`.
 *
 * Pure, so a test can build a fleet by hand. Everything here comes from the
 * snapshot and nothing is recomputed: a state a view re-derived would
 * disagree with the terminal the first time a turn settled while nobody was
 * watching, and it would disagree silently.
 *
 * Every cell the server's own home draws is drawn from the snapshot. The
 * three things it used to leave empty - the per-row working tree, the task a
 * seat holds, and whether an account would bind - cross on `ProjectWire`, as
 * do the unseen marks and the forge version.
 */

import { displayAddress } from '../connect/attempt';
import type {
  AgentRow,
  DictateFailure,
  DictateModelState,
  Gate,
  HomeWire,
  Lifecycle,
  ProjectView,
  ProjectWire,
  Task,
  TaskStatus,
  WireTime,
  WorkState,
} from '../wire/home';
import type { SessionSlot } from '../wire/types';

/** How loud a band card is. */
export type Tone = 'ready' | 'warn' | 'bad' | 'off';

/** One card in the system band. Quiet until it is not. */
export interface BandCard {
  title: string;
  tone: Tone;
  value: string;
  detail: string;
}

/**
 * The state a row draws: the core's lifecycle the snapshot carries, plus the
 * two states that are not the core's to know.
 */
export type RowState =
  | { kind: 'lifecycle'; lifecycle: Lifecycle }
  /** A turn finished on a seat this view was not showing. */
  | { kind: 'unseen' }
  /** A project nothing has ever run in. */
  | { kind: 'never-started' };

/** A row's task, with the artifact it produced. */
export interface TaskCell {
  subject: string;
  chip: string;
  artifact: string | null;
}

/** One row: the same shape for a lead and for a worker. */
export interface Row {
  slot: { org: string; project: string; label: string };
  state: RowState;
  name: string;
  /** The branch and the count, on the rows whose tree the snapshot carries. */
  place: { branch: string | null; files: string | null };
  /**
   * Why the tree could not be read, or `null` when it could. It describes the
   * project's own read, so the lead's row carries it and a worker's does not.
   */
  gate: string | null;
  task: TaskCell | null;
  pending: 'question' | 'permission' | null;
  reason: string | null;
  lastActivity: WireTime | null;
}

/** One org's projects, in the order `forge.toml` declares them. */
export interface OrgSection {
  name: string;
  live: number;
  projects: { lead: Row; workers: Row[]; refused: string | null }[];
}

export interface Header {
  liveAgents: number;
  /**
   * Every project's tasks added up, which is the fleet's total: the snapshot
   * carries every project and each one's whole list, so the sum is what the
   * server's own home summed - not a count of what happens to be on screen.
   */
  tasks: number;
  projects: number;
  installed: string | null;
  /** The version to name when npm has a newer one than the installed CLI. */
  update: string | null;
  /** The forge build serving the socket, which the client draws rather than its own. */
  version: string;
}

export interface HomeView {
  header: Header;
  band: BandCard[];
  orgs: OrgSection[];
}

/**
 * The state a row draws from what the snapshot carries.
 *
 * Two promotions the core does not make, both about what the mark means: a
 * backgrounded task is work even after the turn that started it settled, and
 * a turn that finished while this view was not showing the seat is the one
 * state that answers "what changed while I was away". Neither is computed
 * here - `has_background_work` and `unseen` both cross on the snapshot.
 */
export function stateOf(row: AgentRow, unseen: SessionSlot[]): RowState {
  if (row.lifecycle === 'Idle' && row.has_background_work) {
    return { kind: 'lifecycle', lifecycle: 'Running' };
  }
  if (row.lifecycle === 'Idle' && unseen.some((slot) => sameSlot(slot, row.slot))) {
    return { kind: 'unseen' };
  }
  return { kind: 'lifecycle', lifecycle: row.lifecycle };
}

/** Two slots are the same seat; the session id is the occupant, not the address. */
function sameSlot(a: SessionSlot, b: SessionSlot): boolean {
  return a.org === b.org && a.project === b.project && a.label === b.label;
}

/** The mark a state draws: one class and one dot shape per meaning. */
export function markOf(state: RowState): { class: string; dot: string } {
  if (state.kind === 'unseen') return { class: 'unseen', dot: 'ok' };
  if (state.kind === 'never-started') return { class: 'never', dot: 'off' };
  switch (state.lifecycle) {
    case 'Running':
      return { class: 'running', dot: 'live' };
    case 'Spawning':
      return { class: 'spawning', dot: 'live' };
    case 'Idle':
      return { class: 'idle', dot: 'live' };
    case 'Attention':
      return { class: 'needs', dot: 'warn' };
    case 'AuthRequired':
      return { class: 'auth', dot: 'bad' };
    case 'Failed':
      return { class: 'failed', dot: 'bad' };
    // Both are a session that is not there: the subprocess is gone, or
    // `/logout` took it. One mark, because the row says the same thing.
    case 'Sleeping':
    case 'LoggedOut':
      return { class: 'asleep', dot: 'off' };
  }
}

/** What a held session is waiting on a person for. */
export function waitingOn(pending: 'question' | 'permission'): string {
  return pending === 'question' ? 'asked you a question' : 'a permission prompt is waiting';
}

/** The task-status chip. */
export function chipFor(status: string): string {
  switch (status) {
    case 'in_progress':
      return 'in progress';
    case 'completed':
      return 'done';
    default:
      return status.replace('_', ' ');
  }
}

/**
 * What the artifact column shows: one short token, not the whole thing.
 *
 * A task's artifact is a PR URL or a path, and either would push the row's
 * other cells off a narrow screen, so a PR URL reads `PR 148` and a path
 * reads its file name.
 */
export function artifactLabel(artifact: string): string {
  const trimmed = artifact.replace(/\/+$/, '');
  const parts = trimmed.split('/');
  const last = parts.pop() ?? '';
  const kind = parts.pop() ?? '';
  if (last !== '' && (kind === 'pull' || kind === 'issues')) {
    return `${kind === 'pull' ? 'PR ' : '#'}${last}`;
  }
  return last === '' ? trimmed : last;
}

/**
 * The artifact as a target a browser can follow, or `null` when it is not
 * one: only a whole absolute URL is, since an anchor built from a path or a
 * sentence addresses a relative URL that does not exist.
 */
export function followable(artifact: string): string | null {
  const trimmed = artifact.trim();
  if (/\s/.test(trimmed)) return null;
  const match = /^(https?):\/\/([^/?#]+)/i.exec(trimmed);
  if (!match) return null;
  return (match[2] ?? '') === '' ? null : trimmed;
}

/**
 * The five states that cross as a bare string, as the card's detail words
 * them. A `Record` over the union rather than a `switch`, so a state the
 * core adds is a compile error here and no string ever reaches an `in`.
 */
const MODEL_STATE_WORDS: Record<Extract<DictateModelState, string>, string> = {
  pending: 'waiting',
  verifying: 'verifying',
  fetched: 'fetched',
  loading: 'loading',
  ready: 'loaded',
};

/**
 * One model's state as the card's detail line words it.
 *
 * The lookup's `?? 'failed'` is the total function, not a fallback path:
 * `homeFrom` has already turned a state outside the five into `pending`, so
 * the only way to reach it is a boundary that stopped narrowing - and a card
 * that reads `failed` says so, where a missing entry would draw a blank.
 */
export function modelState(state: DictateModelState): string {
  if (typeof state !== 'string') return 'downloading' in state ? 'fetching' : 'failed';
  return MODEL_STATE_WORDS[state] ?? 'failed';
}

/** Why preflight stopped, in the two words the card has room for. */
export function failureKind(failure: DictateFailure): string {
  return 'hash_mismatch' in failure ? 'hash mismatch' : 'failed';
}

/**
 * The file a failure names, which a hash mismatch is the reason for: the
 * card says which bytes are wrong rather than that something went wrong.
 */
export function failureFile(failure: DictateFailure): string {
  if ('hash_mismatch' in failure) {
    const parts = failure.hash_mismatch.path.replace(/\/+$/, '').split('/');
    return parts[parts.length - 1] ?? failure.hash_mismatch.path;
  }
  if ('cancelled' in failure) return 'stopped';
  return failure.other.message;
}

/** `MAJOR.MINOR.PATCH`, ignoring a `-pre.1` or `+build` suffix on the patch. */
function parseSemverTriple(value: string): [number, number, number] | null {
  const [majorText, minorText, patchText] = value.split('.');
  if (majorText === undefined || minorText === undefined || patchText === undefined) return null;
  const major = Number.parseInt(majorText, 10);
  const minor = Number.parseInt(minorText, 10);
  let digits = '';
  for (const char of patchText) {
    if (char < '0' || char > '9') break;
    digits += char;
  }
  const patch = Number.parseInt(digits, 10);
  if (!Number.isInteger(major) || !Number.isInteger(minor) || !Number.isInteger(patch)) return null;
  return [major, minor, patch];
}

function isStrictlyNewer(lhs: string, rhs: string): boolean {
  const a = parseSemverTriple(lhs);
  const b = parseSemverTriple(rhs);
  if (a === null || b === null) return false;
  const [aMajor, aMinor, aPatch] = a;
  const [bMajor, bMinor, bPatch] = b;
  if (aMajor !== bMajor) return aMajor > bMajor;
  if (aMinor !== bMinor) return aMinor > bMinor;
  return aPatch > bPatch;
}

/**
 * The published version to name, when npm has a newer one than the installed
 * CLI. Both sides are required, so a probe that resolved only one names
 * nothing rather than claiming an update it cannot see.
 */
export function availableVersion(installed: string | null, latest: string | null): string | null {
  if (installed === null || latest === null) return null;
  return isStrictlyNewer(latest, installed) ? latest : null;
}

/** How long ago `at` was, in the shortest unit that reads. */
export function elapsedLabel(at: WireTime, now: number): string {
  const seconds = Math.max(0, Math.floor(now / 1000) - at.secs_since_epoch);
  if (seconds < 60) return 'now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

/**
 * How long ago the session last wrote. A project nothing has run in is
 * `never`; a live session with no transcript yet has only just started,
 * which is `now` rather than an absence.
 */
export function whenOf(row: Row, now: number): string {
  if (row.state.kind === 'never-started') return 'never';
  if (row.lastActivity === null) return 'now';
  return elapsedLabel(row.lastActivity, now);
}

/** The band's four cards, each quiet until its own state says otherwise. */
export function band(wire: HomeWire, address: string): BandCard[] {
  const gateway = wire.accounts.gateway;
  const gatewayCard: BandCard = gateway.bind_error
    ? {
        title: 'gateway',
        tone: 'bad',
        value: `failed :${gateway.port}`,
        detail: gateway.bind_error,
      }
    : gateway.ready
      ? {
          title: 'gateway',
          tone: 'ready',
          value: `bound :${gateway.port}`,
          detail: 'inference listener',
        }
      : { title: 'gateway', tone: 'warn', value: 'binding', detail: 'inference listener' };

  const models = wire.dictate.snapshot.models;
  const failure = wire.dictate.snapshot.failure;
  const dictation: BandCard =
    models.length === 0
      ? { title: 'dictation', tone: 'off', value: 'off', detail: 'enabled = false' }
      : failure !== null
        ? {
            title: 'dictation',
            tone: 'bad',
            value: failureKind(failure),
            detail: failureFile(failure),
          }
        : (() => {
            const ready = models.filter((model) => model.state === 'ready').length;
            return {
              title: 'dictation',
              tone: ready === models.length ? 'ready' : 'warn',
              value: `${ready} of ${models.length} loaded`,
              detail: models.map((model) => modelState(model.state)).join(', '),
            };
          })();

  const ready = wire.accounts.loading.filter((row) => row.state === 'ready').length;
  const bailed = wire.accounts.loading.filter((row) => row.state === 'bailed').length;
  const value = bailed === 0 ? `${ready} ready` : `${ready} ready \u{b7} ${bailed} bailed`;
  // `all_loaded` is the pool having settled AND the listener having bound, so
  // false is not the same as work in progress: with a bind error the card
  // would read "probing" for ever beside a gateway card already saying the
  // address is in use. The terminal names the failure, and this takes its
  // shape. No accounts declared is the third case, where `all_loaded` is
  // vacuously true and a green `0 ready` would claim a pool that is not there.
  const accounts: BandCard =
    gateway.bind_error !== null
      ? { title: 'accounts', tone: 'bad', value, detail: gateway.bind_error }
      : wire.accounts.loading.length === 0
        ? { title: 'accounts', tone: 'off', value: 'none', detail: 'no accounts declared' }
        : {
            title: 'accounts',
            tone: bailed > 0 ? 'bad' : wire.accounts.all_loaded ? 'ready' : 'warn',
            value,
            detail: wire.accounts.all_loaded ? 'probed' : 'probing',
          };

  // The address is the client's own: it connected to this forge, so it is
  // the read of where this page is served from. Drawn as `host:port` rather
  // than the socket URL it is held as, which is what a reader recognises.
  return [
    gatewayCard,
    { title: 'web', tone: 'ready', value: displayAddress(address), detail: 'this page' },
    dictation,
    accounts,
  ];
}

/** How many of an org's projects have a session. */
export function countsOf(org: OrgSection): string {
  const asleep = org.projects.length - org.live;
  if (org.live === 0) return `${asleep} asleep`;
  if (asleep === 0) return `${org.live} live`;
  return `${org.live} live \u{b7} ${asleep} asleep`;
}

/** The project's name and slot, which are how a row is matched to it. */
function keyOf(project: ProjectView): string {
  return `${project.org}\u0000${project.name}`;
}

/**
 * Why a spawn here would be refused, or `null` when it would not be: the
 * two the row cannot run for, told apart by what the wire carries.
 */
export function refusal(hasModel: boolean, wouldBind: boolean): string | null {
  if (!hasModel) return 'no model declared - add `model` to this project';
  if (!wouldBind) return 'no usable accounts';
  return null;
}

/** The lowest millisecond value a row's time carries, for a max over times. */
function toMillis(at: WireTime | null): number | null {
  return at === null ? null : at.secs_since_epoch * 1000 + Math.floor(at.nanos_since_epoch / 1e6);
}

/**
 * The `where` cell's two parts, kept apart because the sheet weights them
 * apart. A count of zero is not a fact about the tree, so it draws nothing.
 */
function placeOf(work: WorkState): Row['place'] {
  const changed = work.changed;
  const files =
    changed === null || changed === 0 ? null : changed === 1 ? '1 file' : `${changed} files`;
  return { branch: work.branch, files };
}

/** What a row says when its working directory is not there to read. */
export function gateLine(gate: Gate): string | null {
  switch (gate) {
    case 'in_repo':
      return null;
    case 'not_a_repository':
      return 'not a git repository, so there is no branch to show';
    case 'gone':
      return 'its working directory is not there';
    case 'scanner_failed':
      return 'its working tree could not be read';
  }
}

/** How close to done a task is, in-progress first. */
function statusRank(status: TaskStatus): number {
  switch (status) {
    case 'in_progress':
      return 0;
    case 'blocked':
      return 1;
    case 'pending':
      return 2;
    case 'completed':
      return 3;
  }
}

/**
 * The task a row shows: the one this label holds that is furthest from done.
 *
 * Picking by position instead would show whatever the store happened to
 * return first, which is insertion order within a run and key order across a
 * restart: a label reused by a new worker can hold the last occupant's
 * finished task, and the row would show it under a state column saying
 * running.
 */
function taskFor(tasks: Task[], label: string): Task | null {
  const held = tasks.filter((task) => task.owner !== null && task.owner.label === label);
  if (held.length === 0) return null;
  return held.reduce((best, task) =>
    statusRank(task.status) < statusRank(best.status) ? task : best,
  );
}

/**
 * One agent's row, from the snapshot and nothing else.
 *
 * `place` and `gate` are passed rather than read, and only the lead's row is
 * handed them: the project's `work` is ONE read built from the lead's seat,
 * so drawing it on a worker's row names a different seat's branch. A blank
 * cell reads as missing; a plausible wrong branch name reads as right, which
 * is why the lead-only rule is worth more than the information it drops.
 */
function rowOf(
  agent: AgentRow,
  name: string,
  wire: ProjectWire,
  unseen: SessionSlot[],
  work: WorkState | null,
): Row {
  const held = taskFor(wire.tasks, agent.label);
  return {
    slot: agent.slot,
    state: stateOf(agent, unseen),
    name,
    place: work === null ? { branch: null, files: null } : placeOf(work),
    gate: work === null ? null : gateLine(work.gate),
    task:
      held === null
        ? null
        : { subject: held.subject, chip: chipFor(held.status), artifact: held.artifact },
    pending: agent.pending,
    reason: agent.reason,
    lastActivity: agent.last_activity,
  };
}

/** The row a project nobody has started gets. */
function dormantRow(wire: ProjectWire, lastRan: WireTime | null): Row {
  return {
    slot: { org: wire.project.org, project: wire.project.name, label: 'lead' },
    state:
      lastRan === null ? { kind: 'never-started' } : { kind: 'lifecycle', lifecycle: 'Sleeping' },
    name: wire.project.name,
    place: placeOf(wire.work),
    gate: gateLine(wire.work.gate),
    task: null,
    pending: null,
    reason: null,
    lastActivity: lastRan,
  };
}

/** Read the snapshot into the shape the markup wants. */
export function homeView(wire: HomeWire, address: string): HomeView {
  const byProject = new Map<string, AgentRow[]>();
  for (const agent of wire.agents) {
    const key = `${agent.slot.org}\u0000${agent.slot.project}`;
    const rows = byProject.get(key);
    if (rows) rows.push(agent);
    else byProject.set(key, [agent]);
  }

  const orgs: OrgSection[] = [];
  for (const row of wire.projects) {
    const project = row.project;
    const rows = byProject.get(keyOf(project)) ?? [];
    const lastRan = project.sessions.reduce<WireTime | null>(
      (latest, session) =>
        toMillis(session.last_activity) !== null &&
        (latest === null || (toMillis(session.last_activity) ?? 0) > (toMillis(latest) ?? 0))
          ? session.last_activity
          : latest,
      null,
    );

    // A project's row is its lead, and the home names it for the project:
    // the lead's label is its identity, not what the row is called here.
    const [head, ...rest] = rows;
    const started = head !== undefined;
    const lead =
      head === undefined
        ? dormantRow(row, lastRan)
        : rowOf(head, project.name, row, wire.unseen, row.work);
    // `null` for a worker: the project's work read is the lead's tree, and a
    // worker's own crosses only once the socket carries one per seat.
    const workers = rest.map((agent) => rowOf(agent, agent.label, row, wire.unseen, null));

    const section = orgs.find((org) => org.name === project.org);
    const entry = {
      lead,
      workers,
      refused: started ? null : refusal(project.has_model, row.would_bind),
    };
    if (section) {
      section.live += started ? 1 : 0;
      section.projects.push(entry);
    } else {
      orgs.push({ name: project.org, live: started ? 1 : 0, projects: [entry] });
    }
  }

  // The orgs read alphabetically rather than in whatever order `forge.toml`
  // declares them; the projects inside each keep their declared order.
  orgs.sort((a, b) => a.name.localeCompare(b.name));

  return {
    header: {
      liveAgents: wire.agents.length,
      tasks: wire.projects.reduce((total, row) => total + row.tasks.length, 0),
      projects: wire.projects.length,
      installed: wire.cli_version?.installed ?? null,
      update: availableVersion(
        wire.cli_version?.installed ?? null,
        wire.cli_version?.latest ?? null,
      ),
      version: wire.forge_version_short,
    },
    band: band(wire, address),
    orgs,
  };
}

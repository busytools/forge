/**
 * The home's reads gathered into the shape its markup wants - the client half
 * of the gather `crates/forge-web/src/home.rs` does in `view_of`.
 *
 * Pure, so a test can build a fleet by hand. Everything here comes from the
 * snapshot and nothing is recomputed: a state a view re-derived would
 * disagree with the terminal the first time a turn settled while nobody was
 * watching, and it would disagree silently.
 *
 * **Four reads the server's own home draws from are not in the snapshot, and
 * the cells they feed are left empty rather than guessed at.** `HomeWire`
 * has no tasks, no per-row working tree, no `would_bind`, and nothing
 * carries forge's own version. See `client/README.md`.
 */

import { displayAddress } from '../connect/attempt';
import type {
  AgentRow,
  DictateFailure,
  DictateModelState,
  HomeWire,
  Lifecycle,
  ProjectView,
  WireTime,
} from '../wire/home';

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
  /** The branch the agent's tree is on, and how much has moved in it. */
  place: { branch: string | null; files: string | null };
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
  projects: number;
  installed: string | null;
  /** The version to name when npm has a newer one than the installed CLI. */
  update: string | null;
}

export interface HomeView {
  header: Header;
  band: BandCard[];
  orgs: OrgSection[];
}

/**
 * The state a row draws from what the snapshot carries.
 *
 * A backgrounded task is work even after the turn that started it settled,
 * which is the one promotion the core does not make. The `unseen` arm is
 * real and drawn - the server's `Live` owns that fact and does not encode it
 * yet, so nothing here computes it and the state arrives as a value like any
 * other.
 */
export function stateOf(row: AgentRow): RowState {
  if (row.lifecycle === 'Idle' && row.has_background_work) {
    return { kind: 'lifecycle', lifecycle: 'Running' };
  }
  return { kind: 'lifecycle', lifecycle: row.lifecycle };
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
  const parts = value.split('.');
  if (parts.length < 3) return null;
  const major = Number.parseInt(parts[0] as string, 10);
  const minor = Number.parseInt(parts[1] as string, 10);
  let digits = '';
  for (const char of parts[2] as string) {
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
  for (let i = 0; i < 3; i += 1) {
    if ((a[i] as number) !== (b[i] as number)) return (a[i] as number) > (b[i] as number);
  }
  return false;
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
    ? { title: 'gateway', tone: 'bad', value: `failed :${gateway.port}`, detail: gateway.bind_error }
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
  const dictation: BandCard = models.length === 0
    ? { title: 'dictation', tone: 'off', value: 'off', detail: 'enabled = false' }
    : failure !== null
      ? { title: 'dictation', tone: 'bad', value: failureKind(failure), detail: failureFile(failure) }
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
  const accounts: BandCard = {
    title: 'accounts',
    tone: bailed > 0 ? 'bad' : wire.accounts.all_loaded ? 'ready' : 'warn',
    value: bailed === 0 ? `${ready} ready` : `${ready} ready \u{b7} ${bailed} bailed`,
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
 * Why a spawn here would be refused, or `null` when it would not be.
 *
 * `has_model` is the snapshot's and decides the first arm outright. The
 * second needs whether an account would bind, which `HomeWire` does not
 * carry, so a project that has a model and no account draws no refusal
 * rather than a guessed one.
 */
export function refusal(project: ProjectView): string | null {
  if (project.has_model) return null;
  return 'no model declared - add `model` to this project';
}

/** The lowest millisecond value a row's time carries, for a max over times. */
function toMillis(at: WireTime | null): number | null {
  return at === null ? null : at.secs_since_epoch * 1000 + Math.floor(at.nanos_since_epoch / 1e6);
}

/** One agent's row, from the snapshot and nothing else. */
function rowOf(agent: AgentRow, name: string): Row {
  return {
    slot: agent.slot,
    state: stateOf(agent),
    name,
    // The working tree is a per-row read the home snapshot does not carry,
    // so the branch and the changed count are absent rather than blank.
    place: { branch: null, files: null },
    task: null,
    pending: agent.pending,
    reason: agent.reason,
    lastActivity: agent.last_activity,
  };
}

/** The row a project nobody has started gets. */
function dormantRow(project: ProjectView, lastRan: WireTime | null): Row {
  return {
    slot: { org: project.org, project: project.name, label: 'lead' },
    state: lastRan === null ? { kind: 'never-started' } : { kind: 'lifecycle', lifecycle: 'Sleeping' },
    name: project.name,
    place: { branch: null, files: null },
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
  for (const project of wire.projects) {
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
    const started = rows.length > 0;
    const lead = started ? rowOf(rows[0] as AgentRow, project.name) : dormantRow(project, lastRan);
    const workers = rows.slice(1).map((agent) => rowOf(agent, agent.label));

    const section = orgs.find((org) => org.name === project.org);
    const entry = { lead, workers, refused: started ? null : refusal(project) };
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
      projects: wire.projects.length,
      installed: wire.cli_version?.installed ?? null,
      update: availableVersion(
        wire.cli_version?.installed ?? null,
        wire.cli_version?.latest ?? null,
      ),
    },
    band: band(wire, address),
    orgs,
  };
}

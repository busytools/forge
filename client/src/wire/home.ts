/**
 * The home's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it, typed against the server's own fixture beside this file.
 *
 * The fixture's `rows` and `crons` arrays are empty, so those two element
 * shapes come from the Rust that emits them rather than from the fixture:
 * the fixture pins what it carries, and the source pins the rest.
 */

import {
  modelStateFrom,
  narrow,
  type DictateFailure,
  type DictateModelState,
  type SessionSlot,
} from './types';

/** `SessionLifecycleState`, as the core's own enum serialises. */
export type Lifecycle =
  | 'Sleeping'
  | 'Spawning'
  | 'Idle'
  | 'Running'
  | 'Attention'
  | 'AuthRequired'
  | 'Failed'
  | 'LoggedOut';

/** `PendingInteractionKind`: the two asks a row can name. */
export type PendingKind = 'question' | 'permission';

/** `forge_gateway::LoadingState`, as the account pool reports it. */
export type LoadingState = 'loading' | 'ready' | 'bailed';

/** `forge_server::work::Gate`: what git said about a row's tree. */
export type Gate = 'in_repo' | 'not_a_repository' | 'gone' | 'scanner_failed';

/** A `SystemTime`, which serde writes as a pair rather than a number. */
export interface WireTime {
  secs_since_epoch: number;
  nanos_since_epoch: number;
}

/** The branch a tree is on, how much moved in it, and whether git ran. */
export interface WorkState {
  branch: string | null;
  changed: number | null;
  gate: Gate;
}

/** A task's status. */
export type TaskStatus =
  'pending' | 'in_progress' | 'waiting' | 'completed' | 'failed' | 'canceled';

/** Why a row cannot proceed, when it is waiting. */
export interface WaitingWire {
  /** `null` only for a row migrated from the old `blocked` spelling. */
  kind: 'decision' | 'dependency' | 'resource' | null;
  detail: string | null;
  /** The task it waits on, for a dependency. */
  on: string | null;
  /** The verify gate: the board's action is approve / send back. */
  verification: boolean;
}

/** What a link points at. Generic to any VCS. */
export type LinkKind = 'spec' | 'plan' | 'issue' | 'pr' | 'branch' | 'path' | 'other';

/** One reference a row carries. */
export interface TaskLinkWire {
  kind: LinkKind;
  label: string | null;
  target: string;
  state: string | null;
  added_at: WireTime;
}

/**
 * One task. `description` and `last_fired`-style fields carry
 * `skip_serializing_if`, so a stored entry written before a field existed
 * arrives without it rather than as null.
 */
export interface Task {
  id: string;
  project_name: string;
  subject: string;
  active_form: string | null;
  detail: string | null;
  status: TaskStatus;
  owner: SessionSlot | null;
  parent: string | null;
  waiting_on: WaitingWire | null;
  estimate: { words: string; secs: number } | null;
  rank: number | null;
  verify: 'user' | 'none' | null;
  links: TaskLinkWire[];
  attempt: number;
  archived_at: WireTime | null;
  created_at: WireTime;
  updated_at: WireTime;
}

/**
 * What the board derives on one row, one boolean per fact. Computed on
 * the server from the history and session liveness - the client draws
 * them, never re-derives them.
 */
export interface Marks {
  /** Pending with nothing waiting on it. */
  ready: boolean;
  /** Has a pull-request link not known to be merged. */
  in_review: boolean;
  /** Its worked time is past its estimate. */
  overdue: boolean;
  /** No touch for longer than the window. */
  no_movement: boolean;
  /** Waiting for longer than the window. */
  waiting_too_long: boolean;
  /** Its owner has no session behind it. */
  stale: boolean;
  /** Every child terminal, its retro run - an epic the lead may close. */
  to_close: boolean;
}

/** One board row: the record plus what the board derived. */
export interface BoardRow {
  task: Task;
  worked_secs: number;
  updated_secs_ago: number;
  marks: Marks;
  /** A parent's children: how many are completed, of how many. */
  rollup: [number, number] | null;
  /** A child's parent subject. */
  parent_subject: string | null;
}

/**
 * One named miss on a fleet row, narrowed once where it enters: the wire
 * sends `"stalled_queue"` or `{"unaccounted_worker": "label"}`, and a
 * shape this client is older than reads as `unknown` rather than being
 * dropped - a miss nobody can see is the failure this board exists to
 * end.
 */
export type MissRow =
  | { kind: 'stalled'; label: string }
  | { kind: 'no-row'; label: string }
  | { kind: 'unknown'; label: string };

/** Narrow one raw miss value from the wire. */
export function missFrom(value: unknown): MissRow {
  if (value === 'stalled_queue') {
    return { kind: 'stalled', label: 'queue is stalling' };
  }
  if (typeof value === 'object' && value !== null && 'unaccounted_worker' in value) {
    const label = (value as { unaccounted_worker?: unknown }).unaccounted_worker;
    if (typeof label === 'string') {
      return { kind: 'no-row', label: `${label} holds no row` };
    }
  }
  return { kind: 'unknown', label: 'a miss this client does not know' };
}

/**
 * One project as the fleet page draws it: the counts and the named
 * misses. A project's own rows ride its [`ProjectWire.rows`]; this is
 * the glance.
 */
export interface FleetRow {
  project: string;
  live_workers: number;
  /** The project's worker cap, resolved (override or default). */
  slots: number | null;
  /** Ready unowned rows. */
  queue: number;
  /** Rows waiting on the user's decision. */
  waiting_on_user: number;
  misses: MissRow[];
}

/** One of a project's schedules. */
export interface CronEntry {
  id: string;
  project_name: string;
  kind: unknown;
  prompt: string;
  description?: string;
  /**
   * The worker label the cron was created by, which is what decides the seat
   * that reads it: `None` targets the project lead, so a row keeps the lead's
   * set or its own label's - never the other's. The same ownership rule the
   * connector subscriptions carry.
   */
  team_role?: string | null;
  created_at: WireTime;
  /** When it is next due, which is the fact the session's schedules section states. */
  next_fire: WireTime;
}

/** One agent as a view reads it: a lead and a worker are the same row. */
export interface AgentRow {
  slot: SessionSlot;
  label: string;
  lifecycle: Lifecycle;
  has_background_work: boolean;
  pending: PendingKind | null;
  pending_depth: number;
  last_activity: WireTime | null;
  reason: string | null;
  /**
   * When the seat's newest turn ended in failure, filtered per view: `null`
   * once this view has shown the seat since the failure, or while it is
   * showing it now. A row that carries it is drawing a failure the reader
   * has not been shown.
   */
  failed_turn: WireTime | null;
  /**
   * The seat's OWN working tree, which is not the project's.
   *
   * A worker's is its worktree and the lead's is the project's path, so a
   * row drawing `ProjectWire.work` instead names a different seat's branch.
   * `null` is a seat forge holds no directory for - a despawned worker's
   * label - and it draws nothing rather than borrowing the project's read.
   */
  work: WorkState | null;
}

/** One project in `forge.toml`, as the roster reports it. */
export interface ProjectView {
  key: string;
  name: string;
  org: string;
  path: string;
  display_path: string;
  accounts: string[];
  fallback_accounts: string[];
  has_model: boolean;
  /**
   * The project's catalog transcripts. Only `last_activity` is read here,
   * which is what tells a project nothing has ever run in from one whose
   * session has gone.
   */
  sessions: { last_activity: WireTime | null }[];
}

/**
 * One project row: the project, and the per-row reads the home draws it
 * from. The reads sit beside the project rather than on a seat, because a
 * home row is a project and a project is a row whether or not anything has
 * started it.
 */
export interface ProjectWire {
  project: ProjectView;
  /** The branch and count for the project's own tree. */
  work: WorkState;
  /** The project's live rows, with the board's own facts on each. */
  rows: BoardRow[];
  crons: CronEntry[];
  /**
   * This project's connector subscriptions, one list per connector.
   *
   * Left `unknown` here like the home's own member and narrowed where it
   * enters: the sets are per project, so they ride the row rather than the
   * home's `connectors`, which carries the liveness facts alone.
   */
  connectors: unknown;
  /** Whether a spawn here would find an account, beside `has_model`. */
  would_bind: boolean;
  /** The account the row chips, and its state. */
  chip: { account_name: string; state: string } | null;
}

export interface GatewayWire {
  ready: boolean;
  port: number;
  bind_error: string | null;
}

export interface AccountLoadingRow {
  display_name: string;
  state: LoadingState;
  last_error: unknown;
  retry_after: unknown;
  auth: string;
}

export interface AccountsWire {
  loading: AccountLoadingRow[];
  all_loaded: boolean;
  gateway: GatewayWire;
  usage: { display_name: string; snapshot: unknown }[];
  orgs: unknown[];
}

export interface DictateModel {
  role: string;
  file: string;
  state: DictateModelState;
}

export interface DictateWire {
  enabled: boolean;
  snapshot: { models: DictateModel[]; failure: DictateFailure | null };
  models_dir: string | null;
}

export interface HomeWire {
  projects: ProjectWire[];
  agents: AgentRow[];
  /** One row per project for the fleet page: the counts and the misses. */
  fleet: FleetRow[];
  /**
   * The seats whose last turn finished while no client was showing them.
   * Nothing in the records can reconstruct this, so it is the one thing a
   * late subscriber cannot draw for itself.
   */
  unseen: SessionSlot[];
  accounts: AccountsWire;
  plugins: { update_records: unknown[] };
  workers: { project: string; workers: unknown[] }[];
  connectors: unknown;
  dictate: DictateWire;
  cli_version: { installed: string | null; latest: string | null } | null;
  /** The forge build serving the socket, which a client draws rather than its own. */
  forge_version: string;
  forge_version_short: string;
  service_status: unknown;
  /** The words of the core's last fatal, which the terminal prints on exit. */
  fatal_error: string | null;
}

/** The lifecycles the core names. A value outside these is one this client is older than. */
const LIFECYCLES: Lifecycle[] = [
  'Sleeping',
  'Spawning',
  'Idle',
  'Running',
  'Attention',
  'AuthRequired',
  'Failed',
  'LoggedOut',
];

const LINK_KINDS: LinkKind[] = ['spec', 'plan', 'issue', 'pr', 'branch', 'path', 'other'];
const VERIFY_VALUES: ('user' | 'none')[] = ['user', 'none'];
const PENDING: PendingKind[] = ['question', 'permission'];
const LOADING: LoadingState[] = ['loading', 'ready', 'bailed'];
const GATES: Gate[] = ['in_repo', 'not_a_repository', 'gone', 'scanner_failed'];
/** Every status the wire carries, for narrowing and for the board's lanes. */
export const TASK_STATUSES: TaskStatus[] = [
  'pending',
  'in_progress',
  'waiting',
  'completed',
  'failed',
  'canceled',
];

/**
 * The snapshot as the types above describe it, with a value outside the
 * shipped set turned into a known one.
 *
 * **The members that are a union of literals are narrowed HERE and not in a
 * renderer.** `markOf`'s switch is exhaustive so that a lifecycle added to
 * the core fails a build until its mark is written; a `default` arm would
 * trade that for a state that silently draws the neutral mark forever. So
 * the unknown value is caught where it enters, and the renderer keeps the
 * compile-time guarantee.
 *
 * A fallback is the least-alarming member rather than the true one: an
 * unknown lifecycle is a client older than its server, and a row that says
 * `idle` is drawn without claiming a state the session may not be in.
 */
export function homeFrom(data: HomeWire): HomeWire {
  return {
    ...data,
    // Absent, or carrying nothing wordable, reads as no fatal: a page must not
    // draw a stopped line it cannot word.
    fatal_error:
      typeof data.fatal_error === 'string' && data.fatal_error !== '' ? data.fatal_error : null,
    agents: data.agents.map((agent) => ({
      ...agent,
      lifecycle: narrow(agent.lifecycle, LIFECYCLES, 'Idle'),
      pending: agent.pending === null ? null : narrow(agent.pending, PENDING, 'permission'),
      // A server old enough not to state the failure leaves the field out,
      // and it must read as `null` rather than as a failure.
      failed_turn: agent.failed_turn ?? null,
      // The gate inside the seat's tree, which is the one member of it that is
      // a union of literals: `WorkState` is a struct, so there is nothing else
      // in it to narrow and a shape test over the object would discriminate
      // nothing. The gate is what a row's `what` cell falls back on.
      work:
        agent.work === null
          ? null
          : { ...agent.work, gate: narrow(agent.work.gate, GATES, 'in_repo') },
    })),
    projects: data.projects.map((row) => ({
      ...row,
      work: { ...row.work, gate: narrow(row.work.gate, GATES, 'in_repo') },
      rows: row.rows.map((entry) => ({
        ...entry,
        task: {
          ...entry.task,
          status: narrow(entry.task.status, TASK_STATUSES, 'pending'),
          // The link kinds and the verify flag are unions of literals, so
          // they are narrowed here like every other union that enters.
          verify:
            entry.task.verify === null ? null : narrow(entry.task.verify, VERIFY_VALUES, 'none'),
          links: entry.task.links.map((link) => ({
            ...link,
            kind: narrow(link.kind, LINK_KINDS, 'other'),
          })),
        },
      })),
    })),
    // The misses are the one member here that is a union of shapes rather
    // than a union of literals, so they are narrowed by `missFrom` rather
    // than by `narrow` - and a shape this client is older than survives as
    // `unknown` rather than vanishing.
    // A server older than the board sends no `fleet`; an empty list draws
    // no rows rather than failing the whole read.
    fleet: (data.fleet ?? []).map((row) => ({
      ...row,
      misses: (row.misses as unknown[]).map(missFrom),
    })),
    accounts: {
      ...data.accounts,
      loading: data.accounts.loading.map((row) => ({
        ...row,
        state: narrow(row.state, LOADING, 'loading'),
      })),
    },
    dictate: {
      ...data.dictate,
      snapshot: {
        ...data.dictate.snapshot,
        models: data.dictate.snapshot.models.map((model) => ({
          ...model,
          state: modelStateFrom(model.state),
        })),
      },
    },
  };
}

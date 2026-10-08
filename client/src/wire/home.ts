/**
 * The home's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it, typed against the server's own fixture beside this file.
 *
 * The fixture's `tasks` and `crons` arrays are empty, so those two element
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
export type TaskStatus = 'pending' | 'in_progress' | 'blocked' | 'completed';

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
  artifact: string | null;
  estimate: string | null;
  created_at: WireTime;
  updated_at: WireTime;
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
  tasks: Task[];
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

const PENDING: PendingKind[] = ['question', 'permission'];
const LOADING: LoadingState[] = ['loading', 'ready', 'bailed'];
const GATES: Gate[] = ['in_repo', 'not_a_repository', 'gone', 'scanner_failed'];
const TASK_STATUSES: TaskStatus[] = ['pending', 'in_progress', 'blocked', 'completed'];

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
      tasks: row.tasks.map((task) => ({
        ...task,
        status: narrow(task.status, TASK_STATUSES, 'pending'),
      })),
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

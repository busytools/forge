/**
 * The home's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it, typed against the server's own fixture beside this file.
 *
 * `homeWire` is the fixture itself. Where this slice renders the home from a
 * store instead, that is the one binding that changes.
 */

import type { SessionSlot } from './types';

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

/** A `SystemTime`, which serde writes as a pair rather than a number. */
export interface WireTime {
  secs_since_epoch: number;
  nanos_since_epoch: number;
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

/** Why preflight stopped, carrying its own reason. */
export type DictateFailure =
  | { hash_mismatch: { path: string; expected: string; actual: string; size: number } }
  | { cancelled: { kept: number; total: number } }
  | { other: { message: string } };

/** How far one model has got. */
export type DictateModelState =
  | 'pending'
  | 'verifying'
  | 'fetched'
  | 'loading'
  | 'ready'
  | { downloading: { downloaded: number; total: number; resumed_from: number | null } }
  | { failed: DictateFailure };

export interface DictateModel {
  role: string;
  file: string;
  state: DictateModelState;
}

export interface DictateWire {
  snapshot: { models: DictateModel[]; failure: DictateFailure | null };
  models_dir: string | null;
}

export interface HomeWire {
  projects: ProjectView[];
  agents: AgentRow[];
  accounts: AccountsWire;
  plugins: { update_records: unknown[] };
  workers: { project: string; workers: unknown[] }[];
  connectors: unknown;
  dictate: DictateWire;
  cli_version: { installed: string | null; latest: string | null } | null;
  service_status: unknown;
  fatal_error: unknown;
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
/** The states that cross as a bare string; the other two are objects. */
const MODEL_STATES: Extract<DictateModelState, string>[] = [
  'pending',
  'verifying',
  'fetched',
  'loading',
  'ready',
];

/**
 * One of `known`, or `fallback` when the value is one this client is older
 * than.
 *
 * The two casts are the boundary's whole job: `known` is read as the strings
 * it holds and the answer is one of them by construction, which is what lets
 * every union downstream stay closed. `Array.includes` cannot narrow, so
 * without them the callers would each need a cast of their own.
 */
function narrow<T extends string>(value: string, known: T[], fallback: T): T {
  return (known as string[]).includes(value) ? (value as T) : fallback;
}

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
    agents: data.agents.map((agent) => ({
      ...agent,
      lifecycle: narrow(agent.lifecycle, LIFECYCLES, 'Idle'),
      pending: agent.pending === null ? null : narrow(agent.pending, PENDING, 'permission'),
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
          state:
            typeof model.state !== 'string'
              ? model.state
              : narrow(model.state, MODEL_STATES, 'pending'),
        })),
      },
    },
  };
}

/**
 * Where a snapshot enters the client: `homeFrom` narrows it, and the fixture
 * that a test or the dev route hands it lives under `src/dev/`, which no
 * shipped build reaches.
 *
 * The fixture's own casts are `resolveJsonModule`'s - it widens a JSON string
 * to `string`, so a member typed as a union of literals cannot be narrowed
 * from the file - and each is applied where that fixture is read, with a line
 * saying so.
 */

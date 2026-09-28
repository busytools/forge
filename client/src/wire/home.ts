/**
 * The home's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it, typed against the server's own fixture beside this file.
 *
 * `homeWire` is the fixture itself. Where this slice renders the home from a
 * store instead, that is the one binding that changes.
 */

import fixture from './home.json';
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

/** `PendingInteractionKind`: the three asks a row can name. */
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

export interface DictateModel {
  role: string;
  file: string;
  state: string;
}

export interface DictateWire {
  snapshot: { models: DictateModel[]; failure: unknown };
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

/**
 * The server's fixture, and the shapes above are checked against it: a field
 * the fixture does not carry is a compile error here rather than a runtime
 * `undefined` on a page.
 *
 * The two casts are `resolveJsonModule`'s: it widens a JSON string to
 * `string`, so the members typed as unions of literals cannot be narrowed
 * from the file. Every OTHER field is checked by the spread, which is the
 * half that catches a fixture drifting away from the types.
 */
export const homeWire: HomeWire = {
  ...fixture,
  agents: fixture.agents as AgentRow[],
  accounts: { ...fixture.accounts, loading: fixture.accounts.loading as AccountLoadingRow[] },
};

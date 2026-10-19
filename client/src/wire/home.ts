/**
 * The home's snapshot, as `crates/forge-server/src/transport/wire.rs` writes
 * it, typed against the server's own fixture beside this file.
 *
 * The fixture's `rows` and `crons` arrays are empty, so those two element
 * shapes come from the Rust that emits them rather than from the fixture:
 * the fixture pins what it carries, and the source pins the rest.
 */

import { reportFrom, type ServiceReport } from './fleet';
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

/** `PendingInteractionKind`: the asks a row can name - the two prompts a
 *  session holds, and a document hand-off parked in the workspace. */
export type PendingKind = 'question' | 'permission' | 'hand_off';

/** `forge_gateway::LoadingState`, as the account pool reports it. */
type LoadingState = 'loading' | 'ready' | 'bailed';

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
  /** The lead parked the wait by design: the chase leaves it alone. */
  parked?: boolean;
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
  /** A lead's record of the look it completed on the user's behalf. */
  attested?: { evidence: string; at: WireTime } | null;
  /** The user reserved this row's pick: the sweep leaves it for them. */
  reserved?: 'user' | null;
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
  /**
   * The row's own transitions, in time order - the detail drawer's history.
   * The server caps what it carries (the newest of them) and `history_total`
   * is the whole count, so the drawer words what it withholds; the withheld
   * tail is not reachable yet (#2242).
   */
  history: BoardTransition[];
  /** How many transitions the row has in all, never fewer than the list above. */
  history_total: number;
}

/**
 * The mover of one transition: a seat, the user, or the core's own
 * machinery (a cleared dependency, the verify gate, archiving). `unknown` is
 * this client's own case, narrowed where the field enters: a `by` a newer
 * server invents reads as an unknown mover rather than as a seat shape this
 * client would dereference into a crash.
 */
export type ByWire = { seat: SessionSlot } | 'user' | 'system' | 'unknown';

/** One status transition of a row, as the board's detail drawer reads it. */
export interface BoardTransition {
  task_id: string;
  project_name: string;
  from: TaskStatus | null;
  to: TaskStatus;
  at: WireTime;
  by: ByWire;
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
   * Whether a session on the seat ended and the reader has not been shown
   * it since: the row's failure is the durable trace of a crash nobody
   * watched, and it is what tells a stalled-from-birth seat (which stays
   * parked) from one that should be started again.
   */
  died: boolean;
  /**
   * The seat's own tree, home-shortened ("~/Projects/nazar"), when the
   * worker runs somewhere other than its project's directory - a worker a
   * lead pointed at a sibling repo. `null` for a lead, for a worker in its
   * project's own tree, and for one the server cannot place, so a row
   * carrying one is naming a tree its project's siblings do not share.
   */
  tree: string | null;
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

interface GatewayWire {
  ready: boolean;
  port: number;
  bind_error: string | null;
}

interface AccountLoadingRow {
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
  connectors: unknown;
  dictate: DictateWire;
  cli_version: { installed: string | null; latest: string | null } | null;
  /** The forge build serving the socket, drawn as the header's `server` pair. */
  forge_version: string;
  forge_version_short: string;
  /**
   * The service status the core holds, which the home draws above the band -
   * `null` when it holds none. It is the probe's own report reaching a late
   * subscriber: the probe fires once, before a client attaches, so a page
   * that only took the report off the update stream drew nothing for an
   * outage that was already up.
   */
  service_status: ServiceReport | null;
  /** The words of the core's last fatal, which the view this client replaced printed on exit. */
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
const PENDING: PendingKind[] = ['question', 'permission', 'hand_off'];
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

/** The marks a row carries when nobody stated any, which is a v7 server's row. */
const NO_MARKS: Marks = {
  ready: false,
  in_review: false,
  overdue: false,
  no_movement: false,
  waiting_too_long: false,
  stale: false,
  to_close: false,
};

/**
 * One entry of a project's `rows` (v8) or `tasks` (v7), as a board row.
 *
 * **The step back is a SHAPE, not only a name.** A v7 server sent the TASK
 * itself, flat: no `worked_secs`, no marks, an `estimate` that is its words
 * alone. A fallback that only renamed the key would read `entry.task.status`
 * off such an entry, and a page that throws while drawing draws nothing at
 * all - which is the failure the protocol bump exists to turn into a notice.
 * So a flat entry is wrapped as the row this client reads, with the neutral
 * value for everything that server never stated.
 */
function rowFrom(entry: unknown): BoardRow {
  const raw = (entry ?? {}) as Record<string, unknown>;
  const flat = raw['task'] === undefined || raw['task'] === null;
  const task = (flat ? raw : (raw['task'] as Record<string, unknown>)) ?? {};
  const history = historyFrom(raw['history']);
  return {
    task: {
      // Spread first, so a field this client is newer than the sender about
      // keeps whatever was sent and is only defaulted where it is absent.
      ...(task as unknown as Task),
      // The unions of literals every read narrows, the link kinds included.
      // The casts are this boundary's own: an entry off a v7 server is a shape
      // this client has no type for, which is the whole reason for the branch.
      //
      // **`blocked` is v7's spelling for a wait.** Narrowed as an unknown it
      // would fall to `pending` and be drawn in the Ready lane as a row a lead
      // dispatches, where the core's own migration of the same value lands on
      // `waiting` with its reason unstated - the board must not read one row
      // two ways depending on which half stepped back.
      status: narrow(statusFrom(task['status']), TASK_STATUSES, 'pending'),
      verify:
        task['verify'] === null || task['verify'] === undefined
          ? null
          : narrow(task['verify'] as string, VERIFY_VALUES, 'none'),
      attested: attestedFrom(task['attested']),
      reserved: task['reserved'] === 'user' ? 'user' : null,
      estimate: estimateFrom(task['estimate']),
      links: linksFrom(task),
      active_form: (task['active_form'] ?? null) as string | null,
      detail: (task['detail'] ?? null) as string | null,
      owner: (task['owner'] ?? null) as SessionSlot | null,
      parent: (task['parent'] ?? null) as string | null,
      waiting_on: waitingFrom(task['waiting_on']),
      rank: (task['rank'] ?? null) as number | null,
      attempt: (task['attempt'] ?? 0) as number,
      archived_at: (task['archived_at'] ?? null) as WireTime | null,
    },
    worked_secs: (raw['worked_secs'] as number | undefined) ?? 0,
    // A v7 entry carries none of the three below: it is a task, and the
    // worked time, the ages and the marks are what the board derives.
    updated_secs_ago: (raw['updated_secs_ago'] as number | undefined) ?? 0,
    marks: (raw['marks'] as Marks | undefined) ?? NO_MARKS,
    rollup: (raw['rollup'] as [number, number] | null | undefined) ?? null,
    parent_subject: (raw['parent_subject'] as string | null | undefined) ?? null,
    // A capture or a server older than the field carries none: an empty
    // history draws the drawer without the section rather than throwing.
    history,
    // A server older than the field carries no count, and the whole trail
    // is what it sent: the list's own length is the total then, which is
    // what `row.history_total > row.history.length` reads as no cap.
    history_total: historyTotalFrom(raw['history_total'], history.length),
  };
}

/** The row's whole transition count, never below what the row carries. */
function historyTotalFrom(raw: unknown, shown: number): number {
  return typeof raw === 'number' && Number.isInteger(raw) && raw >= shown ? raw : shown;
}

/**
 * The status a task entry carries, with v7's own spelling for a wait read as
 * the state this client draws.
 */
function statusFrom(raw: unknown): string {
  const spelled = typeof raw === 'string' ? raw : '';
  return spelled === 'blocked' ? 'waiting' : spelled;
}

/**
 * The references a row carries, and the one a v7 server sent instead.
 *
 * A v8 row carries `links`; a v7 task carried the single `artifact` that field
 * replaced, so it crosses as one link - with the CORE's own classification of
 * a bare target (`LinkKind::for_target`, which its migration of a v7 artifact
 * applies too), because `other` is the one kind both readers drop: the strip's
 * meta and the home row's cell filter it out, so a v7 row would draw no
 * artifact anywhere but the board card.
 */
function linksFrom(task: Record<string, unknown>): TaskLinkWire[] {
  const held = Array.isArray(task['links']) ? task['links'] : [];
  const links = held
    .filter((link): link is Record<string, unknown> => typeof link === 'object' && link !== null)
    .map((link) => {
      const spelled = link as unknown as TaskLinkWire;
      return { ...spelled, kind: narrow(spelled.kind, LINK_KINDS, 'other') };
    });
  const artifact = task['artifact'];
  if (links.length === 0 && typeof artifact === 'string' && artifact !== '') {
    links.push({
      kind: artifact.includes('/pull/') ? 'pr' : 'path',
      label: null,
      target: artifact,
      state: null,
      // Unread on this side; a v7 task never carried one.
      added_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    });
  }
  return links;
}

/**
 * A row's history as this client reads it. **Total, like the links reader**:
 * a payload carrying `history` as anything but an array of objects reads as
 * no history, and each entry's unions are narrowed HERE, at the boundary,
 * exactly as lifecycle and links are - a `to` outside the six states reads
 * as `pending`, and a `from` outside them as `pending` too, which only
 * decides between "created" and "moved" in the drawer's words.
 */
function historyFrom(raw: unknown): BoardTransition[] {
  const held = Array.isArray(raw) ? raw : [];
  return held
    .filter((step): step is Record<string, unknown> => typeof step === 'object' && step !== null)
    .map((step) => {
      const from = step['from'];
      return {
        task_id: typeof step['task_id'] === 'string' ? step['task_id'] : '',
        project_name: typeof step['project_name'] === 'string' ? step['project_name'] : '',
        from:
          from === null || from === undefined
            ? null
            : narrow(statusFrom(from), TASK_STATUSES, 'pending'),
        to: narrow(statusFrom(step['to']), TASK_STATUSES, 'pending'),
        at: (step['at'] ?? { secs_since_epoch: 0, nanos_since_epoch: 0 }) as WireTime,
        by: byFrom(step['by']),
      };
    });
}

/** One transition's mover, narrowed to the shapes this client draws. */
function byFrom(raw: unknown): ByWire {
  if (raw === 'user') return 'user';
  if (raw === 'system') return 'system';
  if (typeof raw === 'object' && raw !== null && 'seat' in raw) {
    return raw as { seat: SessionSlot };
  }
  return 'unknown';
}

/**
 * The estimate as this client reads it. **A v7 server sent its words alone**,
 * and this side draws them with `secs: 0` - which the board's own rule reads
 * as no measure - rather than parsing them back: the grammar is the core's
 * (`Estimate::parse`, which its migration applies to these same words), and a
 * second copy of it here would drift from the first, silently, the first time
 * one of them learned a spelling the other did not.
 */
function estimateFrom(raw: unknown): Task['estimate'] {
  if (typeof raw === 'string') return { words: raw, secs: 0 };
  const held = raw as { words?: unknown; secs?: unknown } | null | undefined;
  if (held === null || held === undefined) return null;
  return typeof held.words === 'string' && typeof held.secs === 'number'
    ? { words: held.words, secs: held.secs }
    : null;
}

/**
 * The lead's attestation, where the row carries one. A record without its
 * quoted evidence is not an attestation this client can draw, so it reads as
 * none rather than as a blank claim.
 */
function attestedFrom(raw: unknown): { evidence: string; at: WireTime } | null {
  if (raw === null || raw === undefined) return null;
  const held = raw as { evidence?: unknown; at?: unknown };
  if (typeof held.evidence !== 'string') return null;
  return { evidence: held.evidence, at: held.at as WireTime };
}

/**
 * A row's wait, with `parked` defaulted: a server older than the field sends
 * none, and "not parked" is what its silence meant.
 */
function waitingFrom(raw: unknown): WaitingWire | null {
  if (raw === null || raw === undefined) return null;
  const held = raw as WaitingWire;
  return { ...held, parked: held.parked === true };
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
    // Absent, or carrying nothing wordable, reads as no report: the banner
    // must not draw a line it cannot word, and a severity outside the two is
    // the warning every producer defaults to.
    service_status: reportFrom(data.service_status),
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
      // Same shape: a server that never states the death must read as no
      // death, not as one nobody has been shown.
      died: agent.died === true,
      // And the tree: unstated or blank reads as no tree, never as a name.
      tree: typeof agent.tree === 'string' && agent.tree !== '' ? agent.tree : null,
      // The gate inside the seat's tree, which is the one member of it that is
      // a union of literals: `WorkState` is a struct, so there is nothing else
      // in it to narrow and a shape test over the object would discriminate
      // nothing. The gate is what a row's `what` cell falls back on.
      work:
        agent.work === null
          ? null
          : { ...agent.work, gate: narrow(agent.work.gate, GATES, 'in_repo') },
    })),
    projects: data.projects.map((row) => {
      // **The one step back.** A forge at protocol 7 sent this project's rows
      // under `tasks`, so a client newer than its server reads the old key
      // rather than drawing nothing. The cast is this boundary's own - the
      // payload is typed as today's shape, and the field a move left behind is
      // what is being read.
      const legacy = row as { rows?: unknown[]; tasks?: unknown[] };
      return {
        ...row,
        work: { ...row.work, gate: narrow(row.work.gate, GATES, 'in_repo') },
        rows: (legacy.rows ?? legacy.tasks ?? []).map(rowFrom),
      };
    }),
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

/**
 * The session record, as `crates/forge-server/src/transport/wire.rs` writes it.
 *
 * The home's own shapes are read from `../wire/home.ts` rather than copied: a
 * project's working tree is one record on both surfaces, and a second shape
 * for it is the drift the wire's own comments warn about.
 *
 * Half of the inspector's nine sections are not on this record and are read
 * from the home's snapshot instead - a project's tasks and its schedules, and
 * the connector views. They are keyed by PROJECT there, which is the grain the
 * home needs; a session subscribes to the home beside its own seat and picks
 * its project out.
 */

import type { WireTime, WorkState } from '../wire/home';
import type { SessionSlot } from '../wire/types';

/** The model the CLI resolved, of which a header draws one name. */
export interface ModelFacts {
  resolved_id: string;
  display_name_long: string;
}

/** How full the session's context window is. Both halves report independently. */
export interface ContextUsage {
  percent: number | null;
  max_tokens: number | null;
}

/**
 * The header's facts.
 *
 * `available_models` and `turn_in_flight` are the two a client cannot reach
 * any other way, and neither is drawn by this shell: the model picker and the
 * live turn are the chat's and the composer's.
 */
export interface SessionHeader {
  model: ModelFacts | null;
  effort: Effort;
  permission_mode: PermissionMode | null;
  context: ContextUsage;
  available_models: unknown[];
  turn_in_flight: boolean;
}

/** `EffortLevel`, as the core's own enum serialises. */
export type Effort = 'low' | 'medium' | 'high' | 'xhigh' | 'max';

/** `PermissionMode`, as its own serde writes it: camelCase, not snake. */
export type PermissionMode =
  'default' | 'acceptEdits' | 'plan' | 'dontAsk' | 'auto' | 'bypassPermissions';

/** `McpServerConnectionStatus`, kebab-case on the wire. */
export type McpConnection = 'connected' | 'failed' | 'needs-auth' | 'pending' | 'disabled';

/** One MCP server, in the CLI's own camelCase shape. */
export interface McpServer {
  name: string;
  status: McpConnection;
  error?: string | undefined;
  scope?: string | undefined;
  config?: unknown;
  tools?: unknown[] | undefined;
}

/** The session's MCP read, and why it failed when it did. */
export interface McpServers {
  servers: McpServer[];
  error: string | null;
}

/** One process the walk found, flat: the tree's shape is `parent_pid`. */
export interface ProcessEntry {
  pid: number;
  parent_pid: number;
  name: string;
  command: string;
  memory_bytes: number;
}

export interface ProcessSnapshot {
  processes: ProcessEntry[];
  scanned_at: WireTime;
}

/** `MonitorStatus`, snake_case on the wire. */
export type MonitorStatus = 'running' | 'stopped' | 'completed' | 'timed_out';

/** One Monitor the session has running or has finished. */
export interface MonitorRecord {
  tool_use_id: string;
  task_id: string | null;
  description: string;
  command: string;
  persistent: boolean;
  timeout_ms: number;
  status: MonitorStatus;
  output_file: string | null;
  ended_at: WireTime | null;
}

/** The frames the transcript replayed, and how many times it has compacted. */
export interface Conversation {
  messages: unknown[];
  compaction_count: number;
}

/**
 * What the seat's composer is doing.
 *
 * `compacting` is narrowed because the conversation column draws a line for
 * it; the other three are the composer's own states and are left as they came,
 * for the component that owns them rather than for every reader to re-narrow.
 */
export interface ComposerState {
  take: unknown;
  notice: unknown;
  compacting: boolean;
  sign_in: unknown;
}

/** One session, as this shell reads it. */
export interface SessionRecord {
  slot: SessionSlot;
  state: { scan_cwd: string };
  header: SessionHeader;
  mcp: McpServers | null;
  processes: ProcessSnapshot | null;
  monitors: MonitorRecord[];
  /** The CLI's background-task registry, which no section of this shell draws. */
  background_tasks: unknown[];
  /** What this seat's composer is doing. */
  composer: ComposerState;
  /** The prompt this seat is waiting on, which the composer's dock draws. */
  pending_ask: unknown;
  conversation: Conversation;
  /** The working tree's branch and count, and whether git could read it. */
  work: WorkState;
  /** The open pull request this seat's branch is on. */
  pr: { number: number; url: string } | null;
  /** The issues that pull request closes. */
  closes: { number: number; url: string }[];
}

const EFFORTS: Effort[] = ['low', 'medium', 'high', 'xhigh', 'max'];
const MODES: PermissionMode[] = [
  'default',
  'acceptEdits',
  'plan',
  'dontAsk',
  'auto',
  'bypassPermissions',
];
const CONNECTIONS: McpConnection[] = ['connected', 'failed', 'needs-auth', 'pending', 'disabled'];
const MONITOR_STATUSES: MonitorStatus[] = ['running', 'stopped', 'completed', 'timed_out'];
const FALLBACK_EFFORT: Effort = 'medium';
const FALLBACK_MODE: PermissionMode = 'default';
const FALLBACK_CONNECTION: McpConnection = 'pending';
const FALLBACK_MONITOR: MonitorStatus = 'running';

/**
 * One of `known`, or `fallback` when the value is one this client is older
 * than.
 *
 * The two casts are the boundary's whole job, and the reason they are here
 * rather than in a renderer: `Array.includes` cannot narrow, so without them
 * every reader of a lifecycle, a monitor state or a connection status would
 * need a cast of its own and no union downstream would stay closed.
 */
function narrow<T extends string>(value: unknown, known: T[], fallback: T): T {
  return typeof value === 'string' && (known as string[]).includes(value) ? (value as T) : fallback;
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function time(value: unknown): WireTime | null {
  const held = record(value);
  const secs = number(held['secs_since_epoch']);
  const nanos = number(held['nanos_since_epoch']);
  return secs === null || nanos === null
    ? null
    : { secs_since_epoch: secs, nanos_since_epoch: nanos };
}

/**
 * The snapshot as the types above describe it.
 *
 * **A value outside the shipped set is narrowed HERE.** A monitor status or a
 * connection state added to the core is a client older than its server, and
 * the least-alarming member is the fallback rather than the true one: a row
 * that says `running` for a state it cannot name draws without claiming the
 * work is over.
 */
export function sessionFrom(data: unknown): SessionRecord {
  const held = record(data);
  const header = record(held['header']);
  const raw = record(header['model']);

  return {
    // The record's own address, read as the triple it is: every command this
    // page dispatches and every update it routes carries one, and the server
    // is the only thing that decides what a valid one looks like.
    slot: held['slot'] as SessionSlot,
    state: { scan_cwd: text(record(held['state'])['scan_cwd']) ?? '' },
    header: {
      model:
        typeof raw['resolved_id'] === 'string'
          ? {
              resolved_id: raw['resolved_id'],
              display_name_long: text(raw['display_name_long']) ?? raw['resolved_id'],
            }
          : null,
      effort: narrow(header['effort'], EFFORTS, FALLBACK_EFFORT),
      permission_mode:
        header['permission_mode'] === null || header['permission_mode'] === undefined
          ? null
          : narrow(header['permission_mode'], MODES, FALLBACK_MODE),
      context: {
        percent: number(record(header['context'])['percent']),
        max_tokens: number(record(header['context'])['max_tokens']),
      },
      available_models: list(header['available_models']),
      turn_in_flight: header['turn_in_flight'] === true,
    },
    mcp: mcpFrom(held['mcp']),
    processes: processesFrom(held['processes']),
    monitors: list(held['monitors']).map(monitorFrom),
    background_tasks: list(held['background_tasks']),
    composer: composerFrom(held['composer']),
    pending_ask: held['pending_ask'] ?? null,
    conversation: {
      messages: list(record(held['conversation'])['messages']),
      compaction_count: number(record(held['conversation'])['compaction_count']) ?? 0,
    },
    work: workFrom(held['work']),
    pr: prFrom(held['pr']),
    closes: list(held['closes'])
      .map(prFrom)
      .filter((issue): issue is { number: number; url: string } => issue !== null),
  };
}

function mcpFrom(value: unknown): McpServers | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  return {
    servers: list(held['servers']).map((entry) => {
      const server = record(entry);
      return {
        name: text(server['name']) ?? '',
        status: narrow(server['status'], CONNECTIONS, FALLBACK_CONNECTION),
        error: text(server['error']) ?? undefined,
        scope: text(server['scope']) ?? undefined,
        config: server['config'],
        tools: Array.isArray(server['tools']) ? (server['tools'] as unknown[]) : undefined,
      };
    }),
    error: text(held['error']),
  };
}

function processesFrom(value: unknown): ProcessSnapshot | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  return {
    processes: list(held['processes']).map((entry) => {
      const process = record(entry);
      return {
        pid: number(process['pid']) ?? 0,
        parent_pid: number(process['parent_pid']) ?? 0,
        name: text(process['name']) ?? '',
        command: text(process['command']) ?? '',
        memory_bytes: number(process['memory_bytes']) ?? 0,
      };
    }),
    scanned_at: time(held['scanned_at']) ?? { secs_since_epoch: 0, nanos_since_epoch: 0 },
  };
}

function composerFrom(value: unknown): ComposerState {
  const held = record(value);
  return {
    take: held['take'] ?? null,
    notice: held['notice'] ?? null,
    compacting: held['compacting'] === true,
    sign_in: held['sign_in'] ?? null,
  };
}

function monitorFrom(value: unknown): MonitorRecord {
  const held = record(value);
  return {
    tool_use_id: text(held['tool_use_id']) ?? '',
    task_id: text(held['task_id']),
    description: text(held['description']) ?? '',
    command: text(held['command']) ?? '',
    persistent: held['persistent'] === true,
    timeout_ms: number(held['timeout_ms']) ?? 0,
    status: narrow(held['status'], MONITOR_STATUSES, FALLBACK_MONITOR),
    output_file: text(held['output_file']),
    ended_at: time(held['ended_at']),
  };
}

function prFrom(value: unknown): { number: number; url: string } | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const count = number(held['number']);
  return count === null ? null : { number: count, url: text(held['url']) ?? '' };
}

const GATES: WorkState['gate'][] = ['in_repo', 'not_a_repository', 'gone', 'scanner_failed'];

function workFrom(value: unknown): WorkState {
  const held = record(value);
  return {
    branch: text(held['branch']),
    changed: number(held['changed']),
    gate: narrow(held['gate'], GATES, 'in_repo'),
  };
}

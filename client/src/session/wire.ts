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
 * One prompt waiting in the CLI's queue - the pile's row.
 *
 * The core holds the set, so two clients agree about it, and the seat read
 * carries it for a client that attached mid-queue. `source` is the wire's own
 * label (`you`, `cron`, `gotify`, `slack`, `peer`, `forge`); an unknown one is
 * kept as it arrived rather than dropped, because a sender this build cannot
 * name still queued a prompt.
 */
export interface QueuedPromptRow {
  uuid: string;
  source: string;
  text: string;
}

/**
 * The last queued prompt that left by something other than being taken or
 * cancelled - a session that ended with it waiting, or a hook that refused it.
 *
 * Held on the record so the pile can say so where the card was: a row that
 * vanishes with no word is indistinguishable from one that was delivered, and
 * the two are not the same thing to a reader.
 */
export interface QueueEnding {
  text: string;
  state: string;
}

/**
 * The header's facts.
 *
 * `available_models` and `turn_in_flight` are the two a client cannot reach
 * any other way, and neither is drawn by this shell: the model picker and the
 * live turn are the chat's and the composer's.
 */
export interface SessionHeader {
  /**
   * The occupant's id, or `null` when there is no occupant to name: nothing
   * started, nothing connected yet, or an id dropped since.
   *
   * Read from the header rather than from the conversation's own frames: a
   * page that attached after the seat connected never heard the `Connected`
   * that named it, and a frame's `session_id` is the turn's fact rather than
   * the seat's.
   */
  session_id: string | null;
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

/**
 * One entry of the CLI's background-task registry, as `forge-primitives`
 * carries it: the row a backgrounded tool call left behind.
 *
 * `task_type` routes the row - `local_bash` is the processes feed, an agent
 * kind belongs to the dispatch rows. `description` is the line the row leads
 * with, as the CLI wrote it, and `command` is what an OS scan adopts a
 * process by: the join between this row and the walk is that command.
 */
export interface BackgroundTask {
  task_id: string;
  task_type: string;
  description: string;
  command: string | null;
  /** The tool call that began the task, from the CLI's `task_started` link. */
  tool_use_id: string | null;
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

/**
 * One turn of the transcript: what names it, and the frames it ran as.
 *
 * `key` is `null` on every turn of a transcript-derived conversation - the name
 * comes from a `Result` frame and a transcript holds none - so it is carried
 * for a live session and must not be keyed on.
 */
export interface Turn {
  key: string | null;
  messages: unknown[];
}

/** The transcript's whole turns, and how many times it has compacted. */
export interface Conversation {
  turns: Turn[];
  compaction_count: number;
}

/**
 * What the seat's composer is doing.
 *
 * `take` and `notice` are this client's own: a take belongs to the
 * connection that started it, so the record carries neither of them - they
 * open empty and only the take's own updates, which this connection alone
 * receives, fill them. `compacting` is narrowed because the composer draws
 * a line for it; `sign_in` is left as it came, for the component that owns
 * it rather than for every reader to re-narrow.
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
  /** The CLI's background-task registry: what this seat has running out of band. */
  background_tasks: BackgroundTask[];
  /**
   * The composer's three lists: the CLI's commands for `/`, the agent types
   * for `&`, and the working tree's files for `@`. Carried rather than
   * narrowed, because the composer's own task owns their shape.
   */
  slash_commands: unknown[];
  subagents: unknown[];
  /** The instances the session dispatched, joined by the core. */
  subagent_instances: SubagentCard[];
  file_index: unknown;
  /** What this seat's composer is doing. */
  composer: ComposerState;
  /** The prompts still waiting in the CLI's queue, oldest first: the pile. */
  queue: QueuedPromptRow[];
  /**
   * The last row that left by discard or refusal, which the pile says in one
   * line where the card was. Not on the wire: the read carries what is
   * waiting, and this is what this view watched leave.
   */
  queue_ended: QueueEnding | null;
  /**
   * The prompts this seat is holding: a draft leads, then arrival order. The
   * composer's dock draws the front; a parallel batch parks two at once, so
   * the record keeps every ask it is told to park rather than a single slot.
   */
  pending_asks: unknown[];
  conversation: Conversation;
  /**
   * Whether this seat's conversation holds a sub-agent dispatch at all.
   *
   * **Parsed, and nothing reads it.** The inspector's subagents section was
   * its only reader and the section is gone - the dispatch rows and the
   * strip's agents row read `subagent_instances` instead. The field stays on
   * the wire: dropping it would be a protocol change, so the parse stays
   * here until the wire itself moves.
   */
  has_dispatches: boolean;
  /** The working tree's branch and count, and whether git could read it. */
  work: WorkState;
  /** The open pull request this seat's branch is on. */
  pr: { number: number; url: string } | null;
  /** The issues that pull request closes. */
  closes: { number: number; url: string }[];
}

/** The axes' vocabularies, as `forge.toml` and the normalizer name them. */
export type DictateStyling = 'casual' | 'semi_casual' | 'semi_formal' | 'formal';
export type DictateStructure = 'prose' | 'lists';
export type DictateContext = 'general' | 'email';

const STYLINGS: DictateStyling[] = ['casual', 'semi_casual', 'semi_formal', 'formal'];
const STRUCTURES: DictateStructure[] = ['prose', 'lists'];
const CONTEXTS: DictateContext[] = ['general', 'email'];

/**
 * One axis, or `null` for a value this client is older than.
 *
 * The least-alarming reading `narrow` takes everywhere else: the panel draws
 * the crate's default rather than a value it cannot name.
 */
function axis<T extends string>(value: unknown, known: T[]): T | null {
  return typeof value === 'string' && (known as string[]).includes(value) ? (value as T) : null;
}

/**
 * The three axes DECIDED: what a capturing client sends with each take, and
 * what the greeting hands it as the values it starts on and resets to.
 *
 * Every axis is a value here rather than a maybe - the crate's own default
 * stands in for anything the server did not send, which is the same value the
 * panel draws as its unset state.
 */
export interface DictateAxes {
  styling: DictateStyling;
  structure: DictateStructure;
  context: DictateContext;
}

/** The crate's own defaults, which is what an absent axis means. */
export const DEFAULT_AXES: DictateAxes = {
  styling: 'semi_formal',
  structure: 'prose',
  context: 'general',
};

/** The axes the greeting carries, narrowed once as they enter. */
export function axesFrom(value: unknown): DictateAxes {
  const held = record(value);
  return {
    styling: axis(held['styling'], STYLINGS) ?? DEFAULT_AXES.styling,
    structure: axis(held['structure'], STRUCTURES) ?? DEFAULT_AXES.structure,
    context: axis(held['context'], CONTEXTS) ?? DEFAULT_AXES.context,
  };
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

/** One turn, with the frames it fails to carry read as none rather than as a
 * shape the row below would have to test for. */
function turnFrom(value: unknown): Turn {
  const held = record(value);
  return { key: text(held['key']), messages: list(held['messages']) };
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
  const state = record(held['state']);

  return {
    // The record's own address, read as the triple it is: every command this
    // page dispatches and every update it routes carries one, and the server
    // is the only thing that decides what a valid one looks like.
    slot: held['slot'] as SessionSlot,
    state: { scan_cwd: text(state['scan_cwd']) ?? '' },
    queue: queuedFrom(state['queue']),
    // A read carries what is waiting, never what left: an ending is this
    // view's own observation, so a seat read starts with none.
    queue_ended: null,
    header: {
      session_id: text(header['session_id']),
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
    background_tasks: list(held['background_tasks']).flatMap(backgroundTaskFrom),
    slash_commands: list(held['slash_commands']),
    subagents: list(held['subagents']),
    subagent_instances: list(held['subagent_instances']).map(subagentCardFrom),
    file_index: held['file_index'] ?? null,
    composer: composerFrom(held['composer']),
    pending_asks: asksFrom(held['pending_asks'], held['pending_ask']),
    conversation: {
      turns: list(record(held['conversation'])['turns']).map(turnFrom),
      compaction_count: number(record(held['conversation'])['compaction_count']) ?? 0,
    },
    has_dispatches: held['has_dispatches'] === true,
    work: workFrom(held['work']),
    pr: prFrom(held['pr']),
    closes: issuesFrom(held['closes']),
  };
}

/**
 * What the seat is holding: the list the socket serves, or the single prompt
 * an older core carried, as a list of one. `[]` when nothing is held.
 *
 * The list wins when both are there, which is what keeps the derived front
 * from being counted twice - a newer core serves the front beside the list it
 * is the first of.
 */
function asksFrom(served: unknown, single: unknown): unknown[] {
  if (Array.isArray(served)) return served;
  return single === null || single === undefined ? [] : [single];
}

/** The pile as the seat read writes it; an entry missing its id or words is not a row. */
function queuedFrom(value: unknown): QueuedPromptRow[] {
  return list(value).flatMap((entry) => {
    const held = record(entry);
    const uuid = text(held['uuid']);
    const words = text(held['text']);
    if (uuid === null || words === null) return [];
    return [{ uuid, source: text(held['source']) ?? 'forge', text: words }];
  });
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

export function processesFrom(value: unknown): ProcessSnapshot | null {
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

/**
 * One registry row, or nothing when it is not a row.
 *
 * A row needs its id, its kind and its words: an entry missing any is not
 * something a view can draw, and it is dropped rather than drawn blank.
 */
export function backgroundTaskFrom(value: unknown): BackgroundTask[] {
  const held = record(value);
  const task_id = text(held['task_id']);
  const task_type = text(held['task_type']);
  const description = text(held['description']);
  if (task_id === null || task_type === null || description === null) return [];
  return [
    {
      task_id,
      task_type,
      description,
      command: text(held['command']),
      tool_use_id: text(held['tool_use_id']),
    },
  ];
}

function composerFrom(value: unknown): ComposerState {
  const held = record(value);
  return {
    // A take and its notice are this client's own, built from the take's
    // updates and never read off a snapshot: the record carries neither.
    take: null,
    notice: null,
    compacting: held['compacting'] === true,
    sign_in: held['sign_in'] ?? null,
  };
}

/** How one of an instance's calls ended, as the wire spells it. */
export type SubagentCallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | 'killed';

const SUBAGENT_CALL_STATUSES: SubagentCallStatus[] = [
  'pending',
  'in_progress',
  'completed',
  'failed',
  'killed',
];

/** One call in an instance's tail. */
export interface SubagentCall {
  name: string;
  title: string;
  status: SubagentCallStatus;
}

/** The usage an instance's last progress frame reported. */
export interface SubagentUsage {
  total_tokens: number;
  tool_uses: number;
  duration_ms: number;
}

/**
 * One sub-agent instance, as the core joined it: a `Task`/`Agent` dispatch
 * with the calls that ran under it.
 *
 * **The join is the server's, not this page's.** A page folds the raw frames
 * for the CHAT, but instances are a join of the frames a dispatch produced
 * (`task_started`, its calls, `task_notification`), and the core hands the
 * joined list over rather than every client re-deriving the liveness ladder.
 */
export interface SubagentCard {
  name: string;
  /** The `tool_use` id of the dispatch that opened it, which joins the card
   * to the chat row the call itself drew. */
  dispatch_id: string;
  agent_type: string | null;
  running: boolean;
  failed: boolean;
  backgrounded: boolean;
  ended_at: WireTime | null;
  calls: number;
  tail: SubagentCall[];
  usage: SubagentUsage | null;
}

/** One instance, narrowed where it enters. */
export function subagentCardFrom(value: unknown): SubagentCard {
  const held = record(value);
  const usage = record(held['usage']);
  // The wire stamps the end as Unix milliseconds; a view's ages read
  // `WireTime`, so the shape is built here rather than at every draw.
  const endedMs = number(held['ended_at_ms']);
  return {
    name: text(held['name']) ?? '',
    dispatch_id: text(held['dispatch_id']) ?? '',
    agent_type: text(held['agent_type']),
    running: held['running'] === true,
    failed: held['failed'] === true,
    backgrounded: held['backgrounded'] === true,
    ended_at:
      endedMs === null
        ? null
        : {
            secs_since_epoch: Math.floor(endedMs / 1000),
            nanos_since_epoch: Math.floor((endedMs % 1000) * 1_000_000),
          },
    calls: number(held['calls']) ?? 0,
    tail: list(held['tail']).map((call) => {
      const heldCall = record(call);
      return {
        name: text(heldCall['name']) ?? '',
        title: text(heldCall['title']) ?? '',
        status: narrow(heldCall['status'], SUBAGENT_CALL_STATUSES, 'pending'),
      };
    }),
    usage:
      held['usage'] === null || held['usage'] === undefined
        ? null
        : {
            total_tokens: number(usage['total_tokens']) ?? 0,
            tool_uses: number(usage['tool_uses']) ?? 0,
            duration_ms: number(usage['duration_ms']) ?? 0,
          },
  };
}

export function monitorFrom(value: unknown): MonitorRecord {
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

/**
 * The issues a pull request closes, as the record holds them.
 *
 * **Exported with the field narrowers beside it, because a pushed frame and a
 * read carry the same three fields**: `work_changed` is narrowed by these very
 * functions, so a field cannot come out one way from a read and another from a
 * frame.
 */
export function issuesFrom(value: unknown): { number: number; url: string }[] {
  return list(value)
    .map(prFrom)
    .filter((issue): issue is { number: number; url: string } => issue !== null);
}

export function prFrom(value: unknown): { number: number; url: string } | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const count = number(held['number']);
  return count === null ? null : { number: count, url: text(held['url']) ?? '' };
}

const GATES: WorkState['gate'][] = ['in_repo', 'not_a_repository', 'gone', 'scanner_failed'];

export function workFrom(value: unknown): WorkState {
  const held = record(value);
  return {
    branch: text(held['branch']),
    changed: number(held['changed']),
    gate: narrow(held['gate'], GATES, 'in_repo'),
  };
}

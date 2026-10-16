/**
 * The session record, as `crates/forge-server/src/transport/wire.rs` writes it.
 *
 * The home's own shapes are read from `../wire/home.ts` rather than copied: a
 * project's working tree is one record on both surfaces, and a second shape
 * for it is the drift the wire's own comments warn about.
 *
 * Half of the strip's rows are not on this record and are read from the
 * home's snapshot instead - a project's tasks and its schedules, and the
 * connector views. They are keyed by PROJECT there, which is the grain the
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
  /**
   * The number the seat's next frame will bear.
   *
   * The point in the seat's own sequence this record stands at, so a client
   * that attaches mid-stream knows which frames it did not miss. The read
   * beside it - `session_id` - is the epoch those numbers belong to: an
   * occupant change starts the count again at 1.
   */
  seq: number;
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

/**
 * One MCP operation that failed, as `SessionUpdate::McpOperationError`
 * carries it: the servers it was about, and the core's words for it.
 */
export interface McpOperationFailure {
  server_name: string | null;
  operation: string;
  message: string;
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
 * `take` is this client's own: a take belongs to the connection that started
 * it, so the record never carries one and only the take's own updates, which
 * this connection alone receives, fill it. `notice` IS on the record when the
 * words are the seat's - a landed take nobody has sent, re-handed so a page
 * that reloaded reads it back (#1360) - and is carried through as it came,
 * like `sign_in`, for the component that owns it. `compacting` is narrowed
 * because the composer draws a line for it.
 */
export interface ComposerState {
  take: unknown;
  notice: unknown;
  compacting: boolean;
  sign_in: unknown;
}

/**
 * One of the seat's review reads: the records, or why they could not be read.
 * The two halves fail apart on the server, so neither is collapsed here, and an
 * outcome this build cannot name is kept as it arrived rather than folded into
 * a failure it is not.
 */
type ReadWire<T> =
  | { outcome: 'read'; value: T }
  | { outcome: 'failed'; value: string }
  | { outcome: 'unknown'; value: unknown };

/** Which side of the diff a thread is anchored to. */
type ReviewSideRecord = 'old' | 'new' | { unknown: string };

/** A thread's lifecycle state. */
export type ReviewStatusRecord =
  'open' | 'addressed' | 'resolved' | 'outdated' | { unknown: string };

/** Who wrote a review turn: the reviewer, an agent by label, or a shape this
 *  build cannot name - kept raw so the row can draw it plainly. */
export type ReviewAuthorRecord =
  { kind: 'user' } | { kind: 'agent'; label: string } | { kind: 'unknown'; raw: unknown };

/** Where a thread is anchored, as the record carries it: a durable location
 *  the resolver re-reads against a fresh scan, not a rendered position. */
export interface ReviewAnchorRecord {
  path: string;
  side: ReviewSideRecord;
  line: number;
  /** The line's FNV-1a hash, as decimal digits: a full u64 has no exact
   *  JSON number in JS, so the wire carries it as a string and the page
   *  echoes it back verbatim on a reply. */
  content_hash: string;
  context: string[];
  base_ref: string;
}

/** One turn in a thread. `review_id` is the round that sealed it, when one has. */
export interface ReviewTurnRecord {
  author: ReviewAuthorRecord;
  text: string;
  at: string;
  review_id: string | null;
}

/** Why an anchor could not be placed, in the resolver's own words. */
type ReviewOutdatedReasonRecord = 'gone' | { ambiguous: { matches: number } };

/**
 * Where a thread's anchor sits in the content the record ships: the server
 * resolved it because this side has no working tree to re-scan.
 *
 * A thread written inside a commit is resolved too - against the branch's
 * content, by content and neighbours rather than by its commit-numbered
 * line - which is what lets the all-changes canvas draw it beside the same
 * code. `null` means this read carries no landing at all: the branch layer
 * the scan could not read, or an anchor numbered against another base. A
 * placed landing never carries `moved_from` for such a thread: that number
 * counts in another scope's numbering.
 */
export type ReviewResolutionRecord =
  | { state: 'placed'; line: number; moved_from: number | null }
  | { state: 'outdated'; reason: ReviewOutdatedReasonRecord }
  | { state: 'unknown'; raw: string };

/** A thread: its anchor, its turns and its state. `commit` scopes it to one
 *  commit's diff; `null` is the whole-branch scope. */
export interface ReviewThreadRecord {
  id: string;
  anchor: ReviewAnchorRecord;
  comments: ReviewTurnRecord[];
  status: ReviewStatusRecord;
  created_at: string;
  updated_at: string;
  commit: string | null;
  resolution: ReviewResolutionRecord | null;
}

/** One submitted round, as the reviewer sealed it. */
export interface ReviewSetRecord {
  id: string;
  number: number;
  summary: string | null;
  created_at: string;
}

/** The seat's review reads, each with its own failure. */
export interface ReviewsRecord {
  threads: ReadWire<ReviewThreadRecord[]>;
  reviews: ReadWire<ReviewSetRecord[]>;
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
   * The MCP refresh that failed last, which the section draws so a list
   * nothing has refreshed is not read as current. Not on the wire: the read
   * carries the servers, and this is what this view watched fail since.
   */
  mcp_operation_error: McpOperationFailure | null;
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
  /** The working tree's branch and count, and whether git could read it. */
  work: WorkState;
  /** The tree behind the git row's depth, as the scan classified it. */
  git: GitWorkRecord;
  /** The open pull request this seat's branch is on. */
  pr: { number: number; url: string; draft: boolean } | null;
  /** The issues that pull request closes. */
  closes: { number: number; url: string }[];
  /** The seat's review threads and submitted rounds, each half with its own
   *  read outcome - the review page's rounds half. */
  reviews: ReviewsRecord;
  /** The working tree's bounded content, which the review page's canvas
   *  draws and its threads' landings were resolved against. */
  diff: DiffRecord;
}

/** How git classified one file's change, as the wire spells it. */
export type FileStatusWire =
  'modified' | 'added' | 'deleted' | 'renamed' | 'copied' | 'typechange' | 'unmerged' | 'untracked';

/** One changed file's mark and counts. */
interface GitFileRecord {
  path: string;
  added: number;
  removed: number;
  status: FileStatusWire;
}

/**
 * One file list with its totals, as the wire carries a layer's or a
 * commit's stats: the full counts, and the busiest files up to the scan's
 * own cap - the count above the list tells the truth either way.
 */
interface GitStatsRecord {
  files: GitFileRecord[];
  totalFiles: number;
  totalAdded: number;
  totalRemoved: number;
}

/**
 * The tree behind the git row's depth: the uncommitted files, the
 * branch's chain ahead of its default - each commit with the files it
 * changed - and which branch that is.
 *
 * A `null` layer is nothing to state - a clean tree, an unscanned one -
 * which is the layer's own meaning on the wire.
 */
export interface GitWorkRecord {
  defaultBranch: string | null;
  worktree: GitStatsRecord | null;
  ahead: {
    count: number;
    commits: {
      sha: string;
      subject: string;
      stats: GitStatsRecord | null;
      /** When it was committed, unix seconds; 0 where the wire stated none. */
      time: number;
    }[];
    stats: GitStatsRecord | null;
  } | null;
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
      seq: number(header['seq']) ?? 1,
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
    // A failed refresh is a moment this view watched, not a fact a read
    // carries: a seat read starts with none.
    mcp_operation_error: null,
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
    work: workFrom(held['work']),
    git: gitFrom(held['git']),
    pr: prFrom(held['pr']),
    closes: issuesFrom(held['closes']),
    reviews: reviewsFrom(held['reviews']),
    diff: diffFrom(held['diff']),
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

/** One half of the reviews read, narrowed at the boundary: a named outcome
 *  carries its records, `failed` its reason, and anything else is kept whole
 *  so a reader can be shown it rather than told it is a failure. */
function readWireFrom<T>(value: unknown, parse: (records: unknown[]) => T[]): ReadWire<T[]> {
  const held = record(value);
  if (held['outcome'] === 'read') return { outcome: 'read', value: parse(list(held['value'])) };
  if (held['outcome'] === 'failed') {
    return { outcome: 'failed', value: text(held['value']) ?? 'the read failed' };
  }
  // An absent half is named rather than carried as `undefined`: "answered
  // undefined" is not a thing a reader can act on.
  return { outcome: 'unknown', value: text(held['outcome']) ?? 'absent' };
}

function reviewAuthorFrom(value: unknown): ReviewAuthorRecord {
  if (value === 'User') return { kind: 'user' };
  const agent = record(record(value)['Agent']);
  const label = text(agent['label']);
  if (label !== null) return { kind: 'agent', label };
  return { kind: 'unknown', raw: value };
}

function reviewSideFrom(value: unknown): ReviewSideRecord {
  if (value === 'Old') return 'old';
  if (value === 'New') return 'new';
  return { unknown: typeof value === 'string' ? value : 'unnamed' };
}

function reviewStatusFrom(value: unknown): ReviewStatusRecord {
  switch (value) {
    case 'Open':
      return 'open';
    case 'Addressed':
      return 'addressed';
    case 'Resolved':
      return 'resolved';
    case 'Outdated':
      return 'outdated';
    default:
      return { unknown: typeof value === 'string' ? value : 'unnamed' };
  }
}

export function reviewThreadFrom(entry: unknown): ReviewThreadRecord {
  const held = record(entry);
  const anchor = record(held['anchor']);
  return {
    id: text(held['id']) ?? '',
    anchor: {
      path: text(anchor['path']) ?? '',
      side: reviewSideFrom(anchor['side']),
      line: number(anchor['line']) ?? 0,
      // Digits, or a number from a server that predates the digits form;
      // either way it crosses back verbatim rather than through a double.
      content_hash:
        text(anchor['content_hash']) ??
        (number(anchor['content_hash']) === null ? '0' : String(anchor['content_hash'])),
      // Context is captured code lines; a non-string entry is not a line.
      context: list(anchor['context']).flatMap((line) => text(line) ?? []),
      base_ref: text(anchor['base_ref']) ?? '',
    },
    comments: list(held['comments']).map((turn) => {
      const comment = record(turn);
      return {
        author: reviewAuthorFrom(comment['author']),
        text: text(comment['text']) ?? '',
        at: text(comment['at']) ?? '',
        review_id: text(comment['review_id']),
      };
    }),
    status: reviewStatusFrom(held['status']),
    created_at: text(held['created_at']) ?? '',
    updated_at: text(held['updated_at']) ?? '',
    commit: text(held['commit']),
    resolution: reviewResolutionFrom(held['resolution']),
  };
}

function reviewResolutionFrom(value: unknown): ReviewResolutionRecord | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  if (held['state'] === 'placed') {
    return {
      state: 'placed',
      line: number(held['line']) ?? 0,
      moved_from: number(held['moved_from']),
    };
  }
  if (held['state'] === 'outdated') {
    const reason = held['reason'];
    const ambiguous = record(reason)['ambiguous'];
    if (ambiguous !== undefined && ambiguous !== null) {
      return {
        state: 'outdated',
        reason: { ambiguous: { matches: number(record(ambiguous)['matches']) ?? 0 } },
      };
    }
    if (reason === 'gone') return { state: 'outdated', reason: 'gone' };
    return { state: 'unknown', raw: 'outdated without a reason this build knows' };
  }
  return { state: 'unknown', raw: typeof held['state'] === 'string' ? held['state'] : 'absent' };
}

export function reviewSetFrom(entry: unknown): ReviewSetRecord {
  const held = record(entry);
  return {
    id: text(held['id']) ?? '',
    number: number(held['number']) ?? 0,
    summary: text(held['summary']),
    created_at: text(held['created_at']) ?? '',
  };
}

export function reviewsFrom(value: unknown): ReviewsRecord {
  const held = record(value);
  return {
    threads: readWireFrom(held['threads'], (records) => records.map(reviewThreadFrom)),
    reviews: readWireFrom(held['reviews'], (records) => records.map(reviewSetFrom)),
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
    // A take is this client's own, built from the take's updates and never
    // read off a snapshot. **The notice is** (#1360): the landed words
    // nobody has sent are re-handed by the record, which is what brings
    // them back to a page that reloaded in between.
    take: null,
    notice: held['notice'] ?? null,
    compacting: held['compacting'] === true,
    sign_in: held['sign_in'] ?? null,
  };
}

/** How one of an instance's calls ended, as the wire spells it. */
type SubagentCallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | 'killed';

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
  // `prFrom` narrows the same two fields; an issue carries no draft, so
  // that one is dropped here rather than inventing a third narrow.
  return list(value).flatMap((entry) => {
    const issue = prFrom(entry);
    return issue === null ? [] : [{ number: issue.number, url: issue.url }];
  });
}

export function prFrom(value: unknown): { number: number; url: string; draft: boolean } | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const count = number(held['number']);
  return count === null
    ? null
    : { number: count, url: text(held['url']) ?? '', draft: held['draft'] === true };
}

const FILE_STATUSES: FileStatusWire[] = [
  'modified',
  'added',
  'deleted',
  'renamed',
  'copied',
  'typechange',
  'unmerged',
  'untracked',
];

/**
 * The tree behind the git row's depth, as the record and a pushed frame
 * carry it - the same fields from both, like the three above.
 */
export function gitFrom(value: unknown): GitWorkRecord {
  const held = record(value);
  const ahead = held['ahead'];
  const aheadHeld = record(ahead);
  const commits = list(aheadHeld['commits']).flatMap((entry) => {
    const commit = record(entry);
    const sha = text(commit['sha']);
    return sha === null
      ? []
      : [
          {
            sha,
            subject: text(commit['subject']) ?? '',
            stats: gitStatsFrom(commit['stats']),
            time: number(commit['time']) ?? 0,
          },
        ];
  });
  return {
    defaultBranch: text(held['default_branch']),
    worktree: gitStatsFrom(held['worktree']),
    ahead:
      ahead === null || ahead === undefined
        ? null
        : {
            count: number(aheadHeld['commit_count']) ?? commits.length,
            commits,
            stats: gitStatsFrom(aheadHeld['stats']),
          },
  };
}

/** One file list with its totals, or `null` where the wire states none. */
function gitStatsFrom(value: unknown): GitStatsRecord | null {
  if (value === null || value === undefined) return null;
  const held = record(value);
  const files = list(held['files']).flatMap((entry) => {
    const file = record(entry);
    const path = text(file['path']);
    return path === null
      ? []
      : [
          {
            path,
            added: number(file['added']) ?? 0,
            removed: number(file['removed']) ?? 0,
            status: narrow(file['status'], FILE_STATUSES, 'modified'),
          },
        ];
  });
  return {
    files,
    totalFiles: number(held['total_files']) ?? files.length,
    totalAdded: number(held['total_added']) ?? 0,
    totalRemoved: number(held['total_removed']) ?? 0,
  };
}

const GATES: WorkState['gate'][] = ['in_repo', 'not_a_repository', 'gone', 'scanner_failed'];

/** How one content layer answered: nothing changed, its files, a scan that
 *  could not read the tree, or a state this build cannot name. */
export type DiffLayerRecord =
  | { state: 'clean' }
  | { state: 'populated'; files: DiffFileRecord[] }
  | { state: 'scan_failed' }
  | { state: 'unknown'; raw: string };

/** A diff line's kind, as the crate spells it. */
export type DiffLineKindRecord = 'context' | 'added' | 'removed';

/** One rendered diff line: its kind, its text with no marker, and its per-side
 *  line numbers. */
export interface DiffLineRecord {
  kind: DiffLineKindRecord;
  text: string;
  oldLine: number | null;
  newLine: number | null;
}

/** One hunk: the two side ranges, and the lines between them. */
export interface DiffHunkRecord {
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: DiffLineRecord[];
}

/** One changed file's bounded content, as the record's diff carries it. */
export interface DiffFileRecord {
  path: string;
  oldPath: string | null;
  status: FileStatusWire;
  binary: boolean;
  submodule: boolean;
  truncated: boolean;
  hunks: DiffHunkRecord[];
}

/** The working tree's bounded content: the uncommitted layer, the branch's
 *  own, and whether the read withheld anything. */
export interface DiffRecord {
  worktree: DiffLayerRecord;
  branchAhead: DiffLayerRecord;
  truncated: boolean;
}

const LINE_KINDS: DiffLineKindRecord[] = ['context', 'added', 'removed'];

/**
 * An absent content key is named rather than folded into an empty layer: a
 * `clean` this build invented and a clean git reported draw the same "no
 * changes" and mean different things.
 */
/**
 * One file's content, as the `file_content` answer carries it: the lines a
 * review page's between-hunk expansion draws, or why the read could not run
 * - never folded into each other, the distinction the diff reader keeps.
 */
export type FileContentRecord =
  | { outcome: 'read'; lines: string[]; truncated: boolean }
  | { outcome: 'failed'; why: string }
  | { outcome: 'unknown'; raw: unknown };

export function fileContentFrom(value: unknown): FileContentRecord {
  const held = record(value);
  const state = held['state'];
  if (state === 'read') {
    return {
      outcome: 'read',
      lines: list(held['lines']).map((line) => text(line) ?? ''),
      truncated: held['truncated'] === true,
    };
  }
  if (state === 'failed') return { outcome: 'failed', why: text(held['why']) ?? '' };
  return { outcome: 'unknown', raw: value };
}

export function diffLayerFrom(value: unknown): DiffLayerRecord {
  if (value === 'clean') return { state: 'clean' };
  if (value === 'scan_failed') return { state: 'scan_failed' };
  const held = record(value);
  const populated = held['populated'];
  if (populated !== undefined)
    return { state: 'populated', files: list(populated).map(diffFileFrom) };
  return { state: 'unknown', raw: typeof value === 'string' ? value : 'absent' };
}

function diffFileFrom(entry: unknown): DiffFileRecord {
  const held = record(entry);
  return {
    path: text(held['path']) ?? '',
    oldPath: text(held['old_path']),
    status: narrow(held['status'], FILE_STATUSES, 'modified'),
    binary: held['binary'] === true,
    submodule: held['submodule'] === true,
    truncated: held['truncated'] === true,
    hunks: list(held['hunks']).map((raw) => {
      const hunk = record(raw);
      return {
        oldStart: number(hunk['old_start']) ?? 0,
        oldCount: number(hunk['old_count']) ?? 0,
        newStart: number(hunk['new_start']) ?? 0,
        newCount: number(hunk['new_count']) ?? 0,
        lines: list(hunk['lines']).map((raw) => {
          const line = record(raw);
          return {
            // The crate's three kinds are pinned by the socket contract; a
            // fourth read as context loses only its marker, not its text.
            kind: narrow(line['kind'], LINE_KINDS, 'context'),
            text: text(line['text']) ?? '',
            oldLine: number(line['old_line']),
            newLine: number(line['new_line']),
          };
        }),
      };
    }),
  };
}

export function diffFrom(value: unknown): DiffRecord {
  const held = record(value);
  return {
    worktree: diffLayerFrom(held['worktree']),
    branchAhead: diffLayerFrom(held['branch_ahead']),
    truncated: held['truncated'] === true,
  };
}

export function workFrom(value: unknown): WorkState {
  const held = record(value);
  return {
    branch: text(held['branch']),
    changed: number(held['changed']),
    gate: narrow(held['gate'], GATES, 'in_repo'),
  };
}

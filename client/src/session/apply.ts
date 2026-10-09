/**
 * What one `SessionUpdate` does to the record a page holds.
 *
 * The server sends the update and the client applies it, which is what the
 * terminal has always done - so a page follows a busy seat without asking for
 * the whole session again on every frame. A read is for a cold load, a
 * reconnect and a seat swap: every slice has a frame.
 *
 * **The record's shape is `crates/forge-server/src/transport/wire.rs`', and the
 * payload's is `crates/forge-workspace/src/protocol.rs`'s `SessionUpdate`. The
 * two name some things differently** - the record's context usage is `percent`
 * and the update's is `percentage` - and nothing compiles the link between
 * them, so every field read here was taken off the Rust side rather than off
 * the record.
 */

import { fold } from '../chat/units';
import { FALLBACK_FLOOR_DB, fractionOf } from '../composer/meter';
import type { SessionUpdate } from '../protocol';
import { METER_CELLS } from '../wire/limits';
import {
  backgroundTaskFrom,
  gitFrom,
  issuesFrom,
  monitorFrom,
  prFrom,
  processesFrom,
  subagentCardFrom,
  workFrom,
  type ComposerState,
  type Conversation,
  type Effort,
  type McpConnection,
  type McpServers,
  type ModelFacts,
  type PermissionMode,
  type SessionHeader,
  type SessionRecord,
} from './wire';

/** `EffortLevel`, as the core's own enum serialises. */
const EFFORTS: Effort[] = ['low', 'medium', 'high', 'xhigh', 'max'];

/** The lifecycle states this build reads as settled - the row leaves the pile. */
const SETTLED_STATES = new Set(['started', 'completed', 'cancelled', 'discarded', 'refused']);

/** `PermissionMode`, as its own serde writes it: camelCase, not snake. */
const MODES: PermissionMode[] = [
  'default',
  'acceptEdits',
  'plan',
  'dontAsk',
  'auto',
  'bypassPermissions',
];

/** What an update does to the record: a patch, or the record unchanged. */
type Apply = (held: SessionRecord, payload: Record<string, unknown>) => SessionRecord;

/**
 * The variants this record has something to do with.
 *
 * A table rather than a `switch`, so the covered set is one readable list and a
 * variant nothing handles is a lookup that misses rather than an arm that
 * falls through.
 */
export const HANDLERS: Record<string, Apply> = {
  /**
   * The CLI the waiting rows were written to is gone.
   *
   * Its queue died with the process, the core clears its own pile on the
   * same event, and no per-row frame will ever say so - so the rows go here,
   * with the ending the design gives anything that leaves without being
   * taken: one dim line where the cards were. Without this a dead worker's
   * cards stand until something else reads the seat, which is the next
   * spawn.
   */
  connection_failed: (held) => {
    if (held.queue.length === 0) return held;
    const last = held.queue[held.queue.length - 1];
    return {
      ...held,
      queue: [],
      queue_ended: last === undefined ? held.queue_ended : { text: last.text, state: 'discarded' },
    };
  },

  chat_appended: (held, payload) => {
    const msg = payload['msg'];
    if (msg === undefined) return held;
    const conversation = appendFrame(held.conversation, msg);
    const composer = compactingOf(held.composer, msg);
    const header = headerFrom(held.header, msg);
    if (
      conversation === held.conversation &&
      composer === held.composer &&
      header === held.header
    ) {
      return held;
    }
    return { ...held, conversation, composer, header };
  },

  hook_observation: (held, payload) => {
    // **The mode and the effort run under live here and nowhere else.** The
    // CLI reports neither on a frame of its own; forge folds both out of a
    // hook payload as it passes through, and hands them on this update. Both
    // are optional there, and a hook that names neither says nothing rather
    // than narrowing the header.
    const mode = knownMode(held.header.permission_mode, payload['permission_mode']);
    const effort = knownEffort(held.header.effort, payload['effort']);
    if (mode === held.header.permission_mode && effort === held.header.effort) return held;
    return { ...held, header: { ...held.header, permission_mode: mode, effort } };
  },

  context_usage_snapshot: (held, payload) => {
    // The update carries both halves as `Option`, so an absent one is a fact
    // about the session rather than a field to keep the old value for; a
    // payload that does not carry the key at all is the other case, and the
    // record is left as it was.
    if (!('percentage' in payload) && !('max_tokens' in payload)) return held;
    const context = {
      percent:
        'percentage' in payload ? number(payload['percentage']) : held.header.context.percent,
      max_tokens:
        'max_tokens' in payload ? number(payload['max_tokens']) : held.header.context.max_tokens,
    };
    return { ...held, header: { ...held.header, context } };
  },

  mcp_snapshot: (held, payload) => {
    const mcp = mcpFrom(payload);
    return mcp === null ? held : { ...held, mcp };
  },

  /**
   * The sets a seat pushes whole, each replacing its field(s) with the frame's
   * own.
   *
   * They are narrowed by the very functions the record's read uses, so a
   * field cannot come out one way from a read and another from a frame.
   */
  work_changed: (held, payload) => ({
    ...held,
    work: workFrom(payload['work']),
    git: gitFrom(payload['git']),
    pr: prFrom(payload['pr']),
    closes: issuesFrom(payload['closes']),
  }),

  monitors_changed: (held, payload) => ({
    ...held,
    monitors: list(payload['monitors']).map(monitorFrom),
  }),

  // The frame names its set `tasks` and the record's field is
  // `background_tasks`, so the payload is read by the frame's own name.
  background_tasks_changed: (held, payload) => ({
    ...held,
    background_tasks: list(payload['tasks']).flatMap(backgroundTaskFrom),
  }),

  // The frame names its set `snapshot` and the record's field is `processes`.
  processes_changed: (held, payload) => {
    const processes = processesFrom(payload['snapshot']);
    return processes === null ? held : { ...held, processes };
  },

  // The CLI's catalogues, each carried whole under the frame's own name. They
  // are the rows `sessionFrom` keeps raw, so the same `list` narrows both.
  slash_commands_changed: (held, payload) => ({
    ...held,
    slash_commands: list(payload['commands']),
  }),

  subagents_changed: (held, payload) => ({
    ...held,
    subagents: list(payload['subagents']),
  }),

  // The joined instance list, carried whole under the frame's own name, with
  // the same narrowers the record read uses.
  subagent_cards_changed: (held, payload) => ({
    ...held,
    subagent_instances: list(payload['cards']).map(subagentCardFrom),
  }),

  // The frame carries the flag's own value. A payload naming no bool is a
  // frame this build cannot read, and it leaves the flag where a dispatch
  // already put it rather than clearing it.
  dispatches_changed: (held, payload) => {
    const dispatched = payload['has_dispatches'];
    return typeof dispatched === 'boolean' ? { ...held, has_dispatches: dispatched } : held;
  },

  // The walk the seat's own loop keeps fresh, carried whole under the frame's
  // own name. A payload naming no index is a frame this build cannot read.
  file_index_changed: (held, payload) => {
    const index = payload['index'];
    return index === undefined ? held : { ...held, file_index: index };
  },

  permission_request: (held, payload) => parked(held, 'permission', payload['request']),

  /**
   * A prompt entered the CLI's queue: the row's words and sender arrive here,
   * where the lifecycle frames carry only the id and the state. The row is
   * keyed by the uuid its sender minted, so a view's own optimistic row and
   * this one are the same row rather than two.
   */
  prompt_queued: (held, payload) => {
    const uuid = text(payload['uuid']);
    const words = text(payload['text']);
    if (uuid === null || words === null) return held;
    if (held.queue.some((row) => row.uuid === uuid)) return held;
    return {
      ...held,
      // A new row is the next thing to look at, so the last ending goes with
      // it rather than standing beside a queue that has moved on.
      queue_ended: null,
      queue: [...held.queue, { uuid, source: text(payload['source']) ?? 'forge', text: words }],
    };
  },

  /**
   * The CLI moved a prompt.
   *
   * The pile holds only prompts still waiting, so a settled state drops the
   * row. **Only a state this build knows settles it**: a word the CLI adds
   * later leaves the row standing, because dropping on a parse miss is the
   * one failure a reader cannot see.
   */
  prompt_lifecycle: (held, payload) => {
    const uuid = text(payload['uuid']);
    const state = text(payload['state']);
    if (uuid === null || state === null || !SETTLED_STATES.has(state)) return held;
    const leaving = held.queue.find((row) => row.uuid === uuid);
    if (leaving === undefined) return held;
    const queue = held.queue.filter((row) => row.uuid !== uuid);
    // Two states leave with a word rather than silently: a session that ended
    // with the prompt waiting, and a hook that refused it. Being taken and
    // being cancelled are the row doing its job, and neither says anything.
    const ending =
      state === 'discarded' || state === 'refused'
        ? { text: leaving.text, state }
        : held.queue_ended;
    return { ...held, queue, queue_ended: ending };
  },

  /**
   * A cancel the CLI confirmed; `cancelled: false` means it had already taken
   * the prompt, and its own `started` frame is what settles the row then.
   */
  prompt_cancel_resolved: (held, payload) => {
    const uuid = text(payload['uuid']);
    if (uuid === null || payload['cancelled'] !== true) return held;
    const queue = held.queue.filter((row) => row.uuid !== uuid);
    return queue.length === held.queue.length ? held : { ...held, queue };
  },

  question_request: (held, payload) => parked(held, 'question', payload['request']),

  pending_interaction_resolved: (held, payload) => {
    // The core's pending set leaves the prompt either way; this is the only
    // thing on the stream that says so, and a view that never held the ask
    // still has one to drop.
    const toolId = text(payload['tool_id']);
    if (toolId === null) return held;
    // **The round as well as the call.** A batch reuses one tool id and
    // advances the question index, and the next round's request can land
    // before this round's resolution - clearing on the id alone dropped the
    // ask that had just parked, so every round after the first lost its
    // opening question (Ved's live find; #1717). A frame naming no round is
    // an older core, and the id is all it can mean there; a permission's
    // frames never name one.
    const index = numberOrNull(payload['question_index']);
    const named = (ask: unknown): boolean => {
      if (askToolId(ask) !== toolId) return false;
      const parked = askIndex(ask);
      return !(index !== null && parked !== null && parked !== index);
    };
    const pending_asks = held.pending_asks.filter((ask) => !named(ask));
    if (pending_asks.length === held.pending_asks.length) return held;
    return { ...held, pending_asks };
  },

  slack_post_pending: (held, payload) => parked(held, 'slack_draft', payload['draft']),

  slack_draft_resolved: (held, payload) => {
    // The draft left the core's registry - answered in another view, expired,
    // or its session gone. A draft is answered by its own id, so this is the
    // only thing on the stream that clears a parked one this view never sent.
    const id = text(payload['id']);
    if (id === null) return held;
    const pending_asks = held.pending_asks.filter((ask) => askKey(ask) !== `slack:${id}`);
    if (pending_asks.length === held.pending_asks.length) return held;
    return { ...held, pending_asks };
  },

  browser_hand_off_pending: (held, payload) => parked(held, 'browser_hand_off', payload['handoff']),

  browser_hand_off_resolved: (held, payload) => {
    // Same shape as the draft's: the hand-off left the core's registry -
    // answered in whichever view, or its asking session gone - and its own id
    // is the only thing on the stream that clears a parked one this view
    // never answered.
    const id = text(payload['id']);
    if (id === null) return held;
    const pending_asks = held.pending_asks.filter((ask) => askKey(ask) !== `handoff:${id}`);
    if (pending_asks.length === held.pending_asks.length) return held;
    return { ...held, pending_asks };
  },

  auth_required: (held, payload) => {
    const method = text(payload['method_name']);
    const description = text(payload['method_description']);
    // A payload naming neither says nothing about how to sign in, and writing
    // blanks would narrow a hint the record already holds.
    if (method === null && description === null) return held;
    return {
      ...held,
      composer: {
        ...held.composer,
        sign_in: {
          method_name: method ?? '',
          method_description: description ?? '',
        },
      },
    };
  },

  dictate_started: (held, payload) => {
    const floor = number(payload['floor_db']);
    if (floor === null) return held;
    // A new take supersedes what the seat was doing, its notice included: the
    // words it left are already in the box.
    return {
      ...held,
      composer: { ...held.composer, take: newTake(floor, payload['generation']), notice: null },
    };
  },

  dictate_level: (held, payload) => {
    const take = heldTake(held.composer);
    const peak = number(payload['peak_db']);
    if (take === null || peak === null) return held;
    return withTake(held, push(take, peak), null);
  },

  dictate_transcribing: (held) => {
    const take = heldTake(held.composer);
    if (take === null) return held;
    return withTake(held, { ...take, phase: 'transcribing' }, null);
  },

  dictate_progress: (held, payload) => {
    const take = heldTake(held.composer);
    if (take === null || !ofThisTake(take, payload)) return held;
    const done = payload['done'];
    const total = payload['total'];
    if (done === undefined && total === undefined) return held;
    return withTake(held, { ...take, progress: [done, total] }, null);
  },

  dictate_ended: (held, payload) => {
    const take = heldTake(held.composer);
    const outcome = outcomeOf(payload['outcome']);
    // **A refusal resolves no take - it answers a start that never ran** -
    // so a take drawn here is this connection's own and still live on the
    // server. Clearing it would empty the screen while the recording runs
    // (#1880); only a seat with nothing drawn takes the refusal's notice.
    if ('refused' in outcome && take !== null) return held;
    // A tail from a take that is gone is not this one.
    if (!('refused' in outcome) && (take === null || !ofThisTake(take, payload))) return held;
    const floor =
      take === null ? FALLBACK_FLOOR_DB : (number(take['floor_db']) ?? FALLBACK_FLOOR_DB);
    return {
      ...held,
      composer: { ...held.composer, take: null, notice: noticeOf(outcome, floor) },
    };
  },

  // The turn's own end, for whichever of the three ways the core says it. All
  // three settle the turn; only `turn_error` reaches the wire today.
  turn_complete: (held) => settled(held),
  turn_cancelled: (held) => settled(held),
  turn_error: (held) => settled(held),
};

/**
 * The variants that replace the seat's whole record rather than patching it: a
 * new occupant under the slot, or the first connect. What they carry is a
 * payload of their own and not a record - the seat's own session facts and the
 * transcript in the folded turns a page reads - so a page answers them with a
 * read.
 */
export const REPLACES: readonly string[] = [
  'spawning',
  'connected',
  'history_replayed',
  'session_replaced',
];

/**
 * The variants that touch nothing on this record.
 *
 * They are the rest of the stream: the fleet's own news, the composer's
 * queued-send bridge, the connector echoes, the plugin and account catalogue.
 * A page hears them because a connection carries every subject's frames, and
 * the record has no field for most of them.
 */
export const IGNORED: readonly string[] = [
  'accounts_changed',
  'call_output',
  'catalog_loaded',
  'cli_version_changed',
  'cron_prompt_appended',
  'dictate_availability',
  'dictate_device_pin',
  // The models page reads the catalogue through its own subject; a
  // session page has nothing of it to draw.
  'dictate_models_changed',
  // The core echoes the terminal's own `/dictate` overrides; this client
  // holds its axes itself, so the echo reaches nothing here.
  'dictate_overrides',
  'fatal_error',
  'forge_account_identity',
  'gotify_notification_appended',
  'mcp_operation_error',
  'oauth_credentials_snapshot',
  'peer_envelope_appended',
  'plugins_cli_action_failed',
  'plugins_cli_action_succeeded',
  'plugins_inventory_refresh_failed',
  'plugins_inventory_updated',
  'plugins_rollback_failed',
  'plugins_rollback_succeeded',
  'plugins_update_run_finished',
  'plugins_update_run_progress',
  'prompt_queued_while_busy',
  // The seat is being released: the record has no field for it, and what a
  // release draws - the row's going-to-sleep mark and the reader's landing -
  // belongs to `session/close.ts`, fed off the same stream.
  'releasing',
  'review_activity_notice',
  'runtime_reload_completed',
  'runtime_reload_failed',
  'service_status',
  'sessions_listed',
  'set_mode_failed',
  'set_model_failed',
  'slash_command_error',
  /**
   * **The conversation this record carries is not the one the page draws.**
   * The page's is `chat/conversation.ts`'s, fed by the live stream, so a line
   * folded in here would reach nothing - and the record has no field of its
   * own for one. That store draws the core's line on arrival and deliberately
   * keeps no copy: the CLI wrote no such row, so a page attaching later has
   * nothing to read it from.
   */
  'notice',
  'slack_message_appended',
  'status_snapshot',
  // The project's task set, its schedules and its connector subscriptions:
  // all three sections are read off the home's project row, and this record
  // has no field for any of them.
  'tasks_changed',
  'cron_schedules_changed',
  'connector_subscriptions_changed',
  'worker_status_changed',
];

/** Fold one update into the record. A variant it has nothing to do with leaves it alone. */
export function applyUpdate(held: SessionRecord, update: SessionUpdate): SessionRecord {
  const [name, payload] = variantOf(update);
  if (name === null) return held;
  const handler = HANDLERS[name];
  return handler === undefined ? held : handler(held, payload);
}

/** The variant's name and its fields, as the core's own externally tagged enums cross. */
export function variantOf(update: SessionUpdate): [string | null, Record<string, unknown>] {
  if (typeof update === 'string') return [update, {}];
  const [name] = Object.keys(update);
  if (name === undefined) return [null, {}];
  return [name, record(update[name])];
}

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' ? (value as Record<string, unknown>) : {};
}

/**
 * A finished take's outcome, keyed.
 *
 * `DictateOutcome` is externally tagged, so a unit variant crosses as its name
 * alone - `cancelled`, `empty`, `failed` - and the rest as a name around their
 * own fields.
 */
function outcomeOf(value: unknown): Record<string, unknown> {
  return typeof value === 'string' ? { [value]: null } : record(value);
}

function text(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function number(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

/** `McpServerConnectionStatus`, kebab-case on the wire. */
const CONNECTIONS: McpConnection[] = ['connected', 'failed', 'needs-auth', 'pending', 'disabled'];

/** The MCP read, narrowed the way the record's own reader narrows the server's. `null` when the payload carries no set at all. */
function mcpFrom(payload: Record<string, unknown>): McpServers | null {
  if (!Array.isArray(payload['servers'])) return null;
  return {
    servers: list(payload['servers']).map((entry) => {
      const server = record(entry);
      return {
        name: text(server['name']) ?? '',
        status: narrow(text(server['status']), CONNECTIONS, 'pending'),
        error: text(server['error']) ?? undefined,
        scope: text(server['scope']) ?? undefined,
        config: server['config'],
        tools: Array.isArray(server['tools']) ? (server['tools'] as unknown[]) : undefined,
      };
    }),
    error: text(payload['error']),
  };
}

/** One of `known`, or `fallback` when the value is one this client is older than. */
function narrow<T extends string>(value: unknown, known: T[], fallback: T): T {
  return typeof value === 'string' && (known as string[]).includes(value) ? (value as T) : fallback;
}

/**
 * The prompt a seat is parked on, placed by the rule every hop keeps: **a
 * draft leads the queue**, and everything else waits oldest first.
 *
 * The draft's precedence is the read's own - it is held in the core's own
 * registry rather than in the session's pending set, and the single read
 * always answered it first - so a fold that appended one would let a re-read
 * flip the front against what the fold holds.
 *
 * **A parallel batch parks two at once**, and a frame carrying one the record
 * already holds is not a second ask: the single slot was idempotent under a
 * re-delivered frame, and the queue stays idempotent the same way.
 */
function parked(held: SessionRecord, kind: string, request: unknown): SessionRecord {
  if (request === undefined) return held;
  const ask = { kind, request };
  const key = askKey(ask);
  if (key !== null && held.pending_asks.some((waiting) => askKey(waiting) === key)) return held;
  // The kinds held in the core's own registries rather than in the session's
  // pending set lead, the way the core's read orders them.
  const leads = (name: unknown): boolean => name === 'slack_draft' || name === 'browser_hand_off';
  if (!leads(kind)) {
    return { ...held, pending_asks: [...held.pending_asks, ask] };
  }
  // Behind the leaders already there, ahead of everything else.
  const firstOther = held.pending_asks.findIndex((waiting) => !leads(record(waiting)['kind']));
  const at = firstOther === -1 ? held.pending_asks.length : firstOther;
  return {
    ...held,
    pending_asks: [...held.pending_asks.slice(0, at), ask, ...held.pending_asks.slice(at)],
  };
}

/**
 * The key an ask is held under: the call, and the round for a question - one
 * tool call carries a whole batch and advances the index, so the id alone
 * would fold two rounds of it into one. A draft and a browser hand-off are
 * each answered by their own id and name no call. `null` for an ask with
 * nothing either could be read from.
 */
function askKey(ask: unknown): string | null {
  const kind = record(ask)['kind'];
  const request = record(record(ask)['request']);
  if (kind === 'slack_draft') {
    const id = text(request['id']);
    return id === null ? null : `slack:${id}`;
  }
  if (kind === 'browser_hand_off') {
    const id = text(request['id']);
    return id === null ? null : `handoff:${id}`;
  }
  const id = text(record(request['tool_call'])['tool_call_id']);
  if (id === null) return null;
  return kind === 'question' ? `question:${id}:${String(askIndex(ask))}` : `permission:${id}`;
}

/** The tool call a parked ask waits on, which is what a resolution names. */
function askToolId(ask: unknown): string | null {
  const request = record(record(ask)['request']);
  return text(record(request['tool_call'])['tool_call_id']);
}

/** The round a parked question asks, in the wire's own name, or null. */
function askIndex(ask: unknown): number | null {
  const request = record(record(ask)['request']);
  return numberOrNull(request['question_index']);
}

/** A finite number, or null for anything else. */
function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

/** The record with the turn settled, or unchanged when it already was. */
function settled(held: SessionRecord): SessionRecord {
  if (!held.header.turn_in_flight) return held;
  return { ...held, header: { ...held.header, turn_in_flight: false } };
}

/**
 * One frame into the turn it belongs to.
 *
 * Where it goes is `client/src/chat/conversation.ts`'s `append`, read there:
 * its predicate is the authority, and this block does not restate it. What
 * follows is where the two differ.
 *
 * **The arms that differ are where a turn BREAKS rather than a second rule.**
 *
 * - With no turn held, EVERY frame opens a row here, where the chat opens one
 *   only for a frame that draws and is not a `system` frame. The chat asks for
 *   a page; this record asks for none.
 * - A drawing frame that is not a `system` frame and not a `user` frame, over
 *   a row with no live turn and no running flag, opens one in the chat where
 *   this record joins it: these turns carry no flag at all.
 * - **A prompt arriving mid-turn opens a turn here, where the chat draws it
 *   inside the row it interrupted: one being written, with no end of its own
 *   in it.** On this arm it is the RECORD that follows the server's fold,
 *   which opens a turn on a queued prompt; the chat refuses that cut and joins
 *   the row to the turn above it.
 *
 * Nothing draws these turns whole: a view reads them flattened, so a
 * difference here is a row count rather than a picture.
 */
function appendFrame(conversation: Conversation, message: unknown): Conversation {
  const last = conversation.turns[conversation.turns.length - 1];
  const opens =
    last === undefined || (fold([message]).length > 0 && !isSystem(message) && opensATurn(message));
  if (opens) {
    return { ...conversation, turns: [...conversation.turns, { key: null, messages: [message] }] };
  }
  if (last === undefined) return conversation;
  return {
    ...conversation,
    turns: [...conversation.turns.slice(0, -1), { ...last, messages: [...last.messages, message] }],
  };
}

/** Whether a frame is a `system` frame, which is a report about a turn rather than part of one. */
function isSystem(message: unknown): boolean {
  return record(message)['type'] === 'system';
}

/** Whether a frame opens a turn of its own rather than joining the live one: what a person said. */
function opensATurn(message: unknown): boolean {
  return record(message)['type'] === 'user';
}

/**
 * What a frame says about the header: the turn, the mode and the model.
 *
 * **`init` is the frame a turn opens with, and the CLI re-fires it at the head
 * of every one**, so it is where a turn beginning is said, and where the model
 * a `/model` switch moved to and the mode a `/mode` moved to arrive. The frame
 * a reader might expect to carry the turn's state - `system/session_state_changed` -
 * is emitted only behind an environment variable forge never sets and appears
 * in none of the committed live captures, so nothing here reads it.
 */
function headerFrom(held: SessionHeader, message: unknown): SessionHeader {
  const frame = record(message);
  const turn = inFlightOf(held.turn_in_flight, frame);
  const opens = frame['type'] === 'system' && frame['subtype'] === 'init';
  const mode = opens
    ? knownMode(held.permission_mode, frame['permissionMode'])
    : held.permission_mode;
  const model = opens ? modelFrom(held, frame['model']) : held.model;
  if (turn === held.turn_in_flight && mode === held.permission_mode && model === held.model) {
    return held;
  }
  return { ...held, turn_in_flight: turn, permission_mode: mode, model };
}

/**
 * What a frame says about a turn being in flight.
 *
 * **One rule, read by both folds**: this record's header fold and the chat's
 * row fold (`chat/conversation.ts`), which used to carry a second copy and
 * could have drifted from this one the day a frame type split them.
 *
 * The rising edge is the frame a turn opens with and the falling edges are its
 * result, whatever the result says, and the error the CLI gives up with, after
 * which no result follows - the rule the core's own turn-commit marker clears
 * on (`crates/forge-workspace/src/session_task.rs`). The third thing the
 * core's own answer is made of is the stamp forge puts on a dispatch of its
 * own, which no update carries: the gap it leaves is the one between a prompt
 * being routed and the CLI's first frame for it, and it closes on that frame.
 */
export function inFlightOf(held: boolean, frame: unknown): boolean {
  const said = record(frame);
  if (said['type'] === 'result' || said['type'] === 'error') return false;
  if (said['type'] === 'system' && said['subtype'] === 'init') return true;
  return held;
}

/**
 * The model a frame names, as the record holds it: the id the CLI gave, and
 * the name the record's own catalogue carries for it.
 *
 * A frame naming the model the session is already on changes nothing, which is
 * the server's own rule (`reconcile_model_from_init`): a session that did not
 * switch keeps the name its connect resolved rather than being renamed to the
 * CLI's spelling of the same model. That rule is mirrored exactly, so the two
 * cannot disagree about WHEN a model is taken.
 *
 * **They can disagree about the NAME, and what this page draws is the id.** Every
 * catalogue row a record can hold is built with its display name set to its id -
 * `AvailableModel::new(model.clone(), model.clone())` is the only construction -
 * so a lookup that hits finds the id again and one that misses leaves the id
 * standing. The server has two ways to arrive at something else: it humanizes an
 * id no row carries, and its lookup accepts a row whose key is merely
 * COMPATIBLE, naming the model where no exact match names it. So the terminal
 * can draw "Opus 5" where this page draws "claude-opus-5".
 * Closing that means porting `humanize_model_id` and the compatibility rule into
 * the client, which this declines on the grounds that the CLI's own spelling of
 * the model beats no name at all.
 */
function modelFrom(held: SessionHeader, value: unknown): ModelFacts | null {
  const id = text(value)?.trim();
  if (id === undefined || id === '' || held.model?.resolved_id === id) return held.model;
  const listed = held.available_models.map(record).find((entry) => entry['id'] === id);
  return { resolved_id: id, display_name_long: text(listed?.['display_name']) ?? '' };
}

/** One of the modes this client knows, or `held` when the payload named none. */
function knownMode(held: PermissionMode | null, value: unknown): PermissionMode | null {
  const mode = text(value);
  return mode !== null && MODES.includes(mode as PermissionMode) ? (mode as PermissionMode) : held;
}

/** One of the efforts this client knows, or `held` when the payload named none. */
function knownEffort(held: Effort, value: unknown): Effort {
  const effort = text(value);
  return effort !== null && EFFORTS.includes(effort as Effort) ? (effort as Effort) : held;
}

/**
 * The compaction a status frame announces and the null that clears it, which
 * is the only place either is said. Mirrors `crates/forge-server/src/composer.rs`,
 * where the same frame is folded for the record's own `composer.compacting`.
 */
function compactingOf(held: ComposerState, message: unknown): ComposerState {
  const frame = record(message);
  if (frame['type'] !== 'system' || frame['subtype'] !== 'status') return held;
  const status = frame['status'];
  if (status === 'compacting') return held.compacting ? held : { ...held, compacting: true };
  if (status === null) return held.compacting ? { ...held, compacting: false } : held;
  return held;
}

/** A take as it begins, as this page's own state holds it. */
function newTake(floorDb: number, generation: unknown): Record<string, unknown> {
  return {
    phase: 'recording',
    levels: [],
    peak_db: floorDb,
    progress: [0, null],
    floor_db: floorDb,
    // The wire carries no duration: the server computes one from the instant
    // its take began, and this is the same reading from this side's clock.
    elapsed_ms: 0,
    started_ms: Date.now(),
    // This side's own bookkeeping, so a report for a superseded take is
    // dropped rather than drawn over the live one. The wire never sends it.
    generation,
  };
}

function heldTake(composer: ComposerState): Record<string, unknown> | null {
  return composer.take === null || typeof composer.take !== 'object'
    ? null
    : (composer.take as Record<string, unknown>);
}

function withTake(
  held: SessionRecord,
  take: Record<string, unknown>,
  notice: Record<string, unknown> | null,
): SessionRecord {
  // The start this page watched stamped the take's own clock; a take that
  // somehow lacks one keeps its own reading of the elapsed time rather than
  // restarting at 0.
  const started = number(take['started_ms']);
  const stamped = started === null ? take : { ...take, elapsed_ms: Date.now() - started };
  return {
    ...held,
    composer: { ...held.composer, take: stamped, notice: notice ?? held.composer.notice },
  };
}

/**
 * Whether a report about a take is about THIS take.
 *
 * A take this connection's `dictate_started` opened carries its generation,
 * so a report naming another one is about a take that is over. A take with
 * no generation at all - a start whose frame was malformed - checks nothing,
 * and a report arriving for it is about it by construction: only one take
 * per seat is live at a time.
 */
function ofThisTake(take: Record<string, unknown>, payload: Record<string, unknown>): boolean {
  const heldGeneration = take['generation'];
  return heldGeneration === undefined || heldGeneration === payload['generation'];
}

/** One reading, as a fraction of the take's own range, keeping the newest cells. */
function push(take: Record<string, unknown>, peakDb: number): Record<string, unknown> {
  const floor = number(take['floor_db']) ?? FALLBACK_FLOOR_DB;
  const fraction = fractionOf(peakDb, floor);
  const levels = list(take['levels']);
  const grown =
    levels.length >= METER_CELLS ? [...levels.slice(1), fraction] : [...levels, fraction];
  return { ...take, levels: grown, peak_db: peakDb };
}

/**
 * The notice a finished take leaves, worded as `crates/forge-server/src/composer.rs:130-161`
 * words it.
 *
 * **The server's own words, mirrored.** The outcome crosses on the update and
 * the wording is built here, but it is the core's vocabulary - a reader who
 * dictates in any view reads the same lines - so four of the notices are
 * literals copied from there, the fifth is the core's own refusal message
 * passed through, and the truncated line a landed take draws is mirrored
 * already by `composer/view.ts`. A drift is a grep away rather than silent.
 */
function noticeOf(
  outcome: Record<string, unknown>,
  floorDb: number,
): Record<string, unknown> | null {
  if ('landed' in outcome) {
    const landed = record(outcome['landed']);
    return {
      kind: 'landed',
      text: text(landed['text']) ?? '',
      truncated: landed['truncated'] === true,
    };
  }
  if ('empty' in outcome) {
    return { kind: 'line', tone: 'q', text: 'that was all filler \u{b7} nothing to insert' };
  }
  if ('no_audio' in outcome) {
    const silent = record(outcome['no_audio']);
    const peak = number(silent['peak_db']);
    if (peak === null) {
      return {
        kind: 'line',
        tone: 'bad',
        text: 'no signal from the microphone at all \u{b7} check permission or mute',
      };
    }
    const seconds = number(silent['seconds']) ?? 0;
    return {
      kind: 'line',
      tone: 'q',
      text: `nothing above ${Math.round(floorDb)} dBFS in ${seconds}s \u{b7} loudest was ${peak.toFixed(1)} \u{b7} try again`,
    };
  }
  if ('refused' in outcome) {
    return {
      kind: 'line',
      tone: 'bad',
      text: text(record(outcome['refused'])['message']) ?? '',
    };
  }
  if ('failed' in outcome) {
    return {
      kind: 'line',
      tone: 'q',
      text: 'dictation failed \u{b7} try again; restart forge if it repeats',
    };
  }
  return null;
}

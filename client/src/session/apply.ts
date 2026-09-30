/**
 * What one `SessionUpdate` does to the record a page holds.
 *
 * The server sends the update and the client applies it, which is what the
 * terminal has always done - so a page follows a busy seat without asking for
 * the whole session again on every frame. A read is for a cold load, a
 * reconnect, and the slices no update carries.
 *
 * **The record's shape is `crates/forge-server/src/transport/wire.rs`', and the
 * payload's is `crates/forge-workspace/src/protocol.rs`'s `SessionUpdate`. The
 * two name some things differently** - the record's context usage is `percent`
 * and the update's is `percentage` - and nothing compiles the link between
 * them, so every field read here was taken off the Rust side rather than off
 * the record.
 */

import { fold } from '../chat/units';
import type { SessionUpdate } from '../protocol';
import type { ComposerState, Conversation, McpConnection, McpServers, SessionRecord } from './wire';

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
  chat_appended: (held, payload) => {
    const msg = payload['msg'];
    if (msg === undefined) return held;
    const conversation = appendFrame(held.conversation, msg);
    const composer = compactingOf(held.composer, msg);
    const turn = inFlightOf(held.header.turn_in_flight, msg);
    if (
      conversation === held.conversation &&
      composer === held.composer &&
      turn === held.header.turn_in_flight
    ) {
      return held;
    }
    return {
      ...held,
      conversation,
      composer,
      header: { ...held.header, turn_in_flight: turn },
    };
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

  permission_request: (held, payload) => parked(held, 'permission', payload['request']),
  question_request: (held, payload) => parked(held, 'question', payload['request']),

  pending_interaction_resolved: (held, payload) => {
    // The core's pending set leaves the prompt either way; this is the only
    // thing on the stream that says so, and a view that never held the ask
    // still has one to drop.
    const toolId = text(payload['tool_id']);
    if (toolId === null || askToolId(held.pending_ask) !== toolId) return held;
    return { ...held, pending_ask: null };
  },

  slack_post_pending: (held, payload) => parked(held, 'slack_draft', payload['draft']),

  slack_draft_expired: (held, payload) => {
    const id = text(payload['id']);
    const heldDraft = record(record(held.pending_ask)['request'])['id'];
    if (id === null || heldDraft !== id) return held;
    return { ...held, pending_ask: null };
  },

  auth_required: (held, payload) => ({
    ...held,
    composer: {
      ...held.composer,
      sign_in: {
        method_name: text(payload['method_name']) ?? '',
        method_description: text(payload['method_description']) ?? '',
      },
    },
  }),

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
    if (take === null || take['generation'] !== payload['generation']) return held;
    return withTake(held, { ...take, progress: [payload['done'], payload['total']] }, null);
  },

  dictate_ended: (held, payload) => {
    const take = heldTake(held.composer);
    const outcome = outcomeOf(payload['outcome']);
    // A refusal resolves no take - it is the answer to one that never ran -
    // and a tail from a take that is gone is not this one.
    if (take === null && !('refused' in outcome)) return held;
    if (take !== null && !('refused' in outcome) && take['generation'] !== payload['generation']) {
      return held;
    }
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
 * payload of their own and not a record - the transcript in the folded turns a
 * page reads, the process walk, the working tree - so a page answers them with
 * a read.
 */
export const REPLACES: readonly string[] = ['spawning', 'connected', 'session_replaced'];

/**
 * The variants that touch nothing on this record.
 *
 * They are the rest of the stream: the fleet's own news, the composer's
 * queued-send bridge, the connector echoes, the plugin and account catalogue.
 * A page hears them because a connection carries every subject's frames, and
 * the record has no field for any of them.
 */
export const IGNORED: readonly string[] = [
  'accounts_changed',
  'catalog_loaded',
  'cli_version_changed',
  'connection_failed',
  'cron_prompt_appended',
  'dictate_availability',
  'dictate_device_pin',
  'dictate_overrides',
  'fatal_error',
  'forge_account_identity',
  'gotify_notification_appended',
  'hook_observation',
  'mcp_operation_error',
  'oauth_credentials_snapshot',
  'peer_envelope_appended',
  'peer_inflight_stats_changed',
  'plugins_cli_action_failed',
  'plugins_cli_action_succeeded',
  'plugins_inventory_refresh_failed',
  'plugins_inventory_updated',
  'plugins_rollback_failed',
  'plugins_rollback_succeeded',
  'plugins_update_run_finished',
  'plugins_update_run_progress',
  'prompt_queued_while_busy',
  'review_activity_notice',
  'runtime_reload_completed',
  'runtime_reload_failed',
  'service_status',
  'sessions_listed',
  'set_mode_failed',
  'set_model_failed',
  'slash_command_error',
  'slack_message_appended',
  'spawning',
  'status_snapshot',
  'worker_status_changed',
];

/**
 * The fields no update feeds, so only a read can move them.
 *
 * A walk of the process table, the working tree and the pull request on its
 * branch, a Monitor's status, the CLI's background-task registry, and the
 * composer's three lists - none of them is carried by any variant of
 * `SessionUpdate`. They are the slowest-moving part of the record: a git scan
 * and a process walk do not change between one frame and the next.
 */
export const UNFED: readonly (keyof SessionRecord)[] = [
  'processes',
  'work',
  'pr',
  'closes',
  'monitors',
  'background_tasks',
  'slash_commands',
  'subagents',
  'file_index',
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

/** The prompt a seat is parked on, keyed the way the record's own reader keys it. */
function parked(held: SessionRecord, kind: string, request: unknown): SessionRecord {
  if (request === undefined) return held;
  return { ...held, pending_ask: { kind, request } };
}

/** The tool call a parked ask waits on, which is what a resolution names. */
function askToolId(ask: unknown): string | null {
  const request = record(record(ask)['request']);
  return text(record(request['tool_call'])['tool_call_id']);
}

/** The record with the turn settled, or unchanged when it already was. */
function settled(held: SessionRecord): SessionRecord {
  if (!held.header.turn_in_flight) return held;
  return { ...held, header: { ...held.header, turn_in_flight: false } };
}

/**
 * One frame into the turn it belongs to.
 *
 * The boundary is `client/src/chat/conversation.ts`'s `append`, which is the
 * chat's own copy of the server's rule: what a person said opens a turn and
 * everything else joins the one already open, and a frame the fold draws
 * nothing out of is never a boundary whatever its type. One arm of that rule
 * is not mirrored - the chat separates a turn a page wrote from one the frames
 * are still writing, and these turns carry no such flag. The difference is
 * only where a turn breaks, and nothing draws these turns whole: the inspector
 * reads them flattened.
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
 * What a frame says about a turn being in flight.
 *
 * Two of the three things the core's own answer is made of - the runtime state
 * and the turn's result - and both ride the frame. The third is the stamp
 * forge puts on a dispatch of its own, which no update carries: the gap it
 * leaves is the one between a prompt being routed and the CLI's first frame
 * for it, and it closes on that frame.
 */
function inFlightOf(held: boolean, message: unknown): boolean {
  const frame = record(message);
  if (frame['type'] === 'result') return false;
  if (frame['type'] !== 'system' || frame['subtype'] !== 'session_state_changed') return held;
  return frame['state'] === 'running' || frame['state'] === 'requires_action';
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

/** How many readings the meter keeps, as the server's own fold caps them. */
const METER_CELLS = 120;

/** The top of the meter's scale in dBFS, which a reading is measured against. */
const METER_CEILING_DB = 0;

/** The floor a take with none of its own is measured against, as the server's fold falls back. */
const FALLBACK_FLOOR_DB = -50;

/** A take as it begins, in the shape the record's own reader narrows. */
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
    // dropped rather than drawn over the live one. The record's reader does
    // not look at it and the wire never sends it.
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
  const elapsed = number(take['started_ms']);
  const stamped = { ...take, elapsed_ms: elapsed === null ? 0 : Date.now() - elapsed };
  return {
    ...held,
    composer: { ...held.composer, take: stamped, notice: notice ?? held.composer.notice },
  };
}

/** One reading, as a fraction of the take's own range, keeping the newest cells. */
function push(take: Record<string, unknown>, peakDb: number): Record<string, unknown> {
  const floor = number(take['floor_db']) ?? FALLBACK_FLOOR_DB;
  const span = Math.max(METER_CEILING_DB - floor, 1);
  const fraction = Math.min(Math.max((peakDb - floor) / span, 0), 1);
  const levels = list(take['levels']);
  const grown =
    levels.length >= METER_CELLS ? [...levels.slice(1), fraction] : [...levels, fraction];
  return { ...take, levels: grown, peak_db: peakDb };
}

/**
 * The notice a finished take leaves, worded as `crates/forge-server/src/composer.rs`
 * words it - the record's notice is that one, and a second wording here would
 * be a second answer to the same question.
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

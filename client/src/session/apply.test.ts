import { describe, expect, it } from 'vitest';

import { applyUpdate, UNFED } from './apply';
import { sessionFrom, type SessionRecord } from './wire';

/**
 * The payloads here are the shapes the workspace actually serialises, read off
 * `crates/forge-workspace/src/protocol.rs` - `percentage` rather than the
 * record's `percent`, `msg` for the frame a `ChatAppended` carries, and the
 * externally tagged `{landed: {...}}` for a dictate outcome. A test built from
 * a guessed field name passes against a reducer that reads a name nothing
 * sends, which is how the chat's own fold went wrong.
 */

const SLOT = { org: 'Busytools', project: 'forge', label: 'lead' };

/** A record as the server answers it, with nothing in it yet. */
function empty(): SessionRecord {
  return sessionFrom({ slot: SLOT, state: { scan_cwd: '/tmp' } });
}

/** The take the record holds, as the reducer wrote it: the record's own reader narrows it. */
function take(record: SessionRecord): Record<string, unknown> {
  return (record.composer.take ?? {}) as Record<string, unknown>;
}

/** The frame the TUI's own tests build, which is what the CLI prints on a result. */
function result(isError = false): Record<string, unknown> {
  return {
    type: 'result',
    subtype: isError ? 'error_during_execution' : 'success',
    duration_ms: 1,
    duration_api_ms: 1,
    is_error: isError,
    num_turns: 1,
    session_id: 's',
  };
}

function state(state: string): Record<string, unknown> {
  return { type: 'system', subtype: 'session_state_changed', session_id: 's', state };
}

/** A prompt as the CLI writes one, which is the frame shape the dev fixture carries. */
function prompt(text: string, uuid = 'u1'): Record<string, unknown> {
  return {
    type: 'user',
    uuid,
    message: { role: 'user', content: [{ type: 'text', text }] },
    session_id: 's',
  };
}

describe('applyUpdate', () => {
  describe('the conversation', () => {
    it('appends a chat_appended frame to the turn it belongs to', () => {
      const held = empty();
      const next = applyUpdate(held, {
        chat_appended: {
          key: SLOT,
          msg: {
            type: 'assistant',
            uuid: 'u1',
            message: { role: 'assistant', content: [{ type: 'text', text: 'hi' }] },
          },
        },
      });

      expect(next.conversation.turns).toHaveLength(1);
      expect(next.conversation.turns[0]?.messages).toHaveLength(1);
    });

    it('opens a turn for what a person said, and joins the answer to it', () => {
      const spoken = applyUpdate(empty(), {
        chat_appended: { key: SLOT, msg: prompt('hello') },
      });
      const answered = applyUpdate(spoken, {
        chat_appended: {
          key: SLOT,
          msg: {
            type: 'assistant',
            uuid: 'u2',
            message: { role: 'assistant', content: [{ type: 'text', text: 'hi' }] },
          },
        },
      });

      expect(answered.conversation.turns).toHaveLength(1);
      expect(answered.conversation.turns[0]?.messages).toHaveLength(2);
    });

    it('answers a second prompt with a turn of its own', () => {
      const first = applyUpdate(empty(), { chat_appended: { key: SLOT, msg: prompt('one') } });
      const second = applyUpdate(first, { chat_appended: { key: SLOT, msg: prompt('two') } });

      expect(second.conversation.turns).toHaveLength(2);
    });

    it('joins a tool result the fold draws nothing out of', () => {
      const held = applyUpdate(empty(), { chat_appended: { key: SLOT, msg: prompt('one') } });
      const next = applyUpdate(held, {
        chat_appended: {
          key: SLOT,
          msg: {
            type: 'user',
            uuid: 'u2',
            message: {
              role: 'user',
              content: [{ type: 'tool_result', tool_use_id: 'tu1', content: 'a\nb' }],
            },
          },
        },
      });

      expect(next.conversation.turns).toHaveLength(1);
      expect(next.conversation.turns[0]?.messages).toHaveLength(2);
    });

    it('keeps a system frame inside the turn it arrived in', () => {
      const held = applyUpdate(empty(), { chat_appended: { key: SLOT, msg: prompt('hello') } });
      const next = applyUpdate(held, {
        chat_appended: {
          key: SLOT,
          msg: { type: 'system', subtype: 'init', session_id: 's', tools: [] },
        },
      });

      expect(next.conversation.turns).toHaveLength(1);
      expect(next.conversation.turns[0]?.messages).toHaveLength(2);
    });
  });

  describe('the turn in flight', () => {
    it('closes on a result frame', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, { chat_appended: { key: SLOT, msg: result() } });

      expect(next.header.turn_in_flight).toBe(false);
    });

    it('opens on the CLI saying the session is running, and closes on idle', () => {
      const running = applyUpdate(empty(), {
        chat_appended: { key: SLOT, msg: state('running') },
      });
      expect(running.header.turn_in_flight).toBe(true);

      const idle = applyUpdate(running, { chat_appended: { key: SLOT, msg: state('idle') } });
      expect(idle.header.turn_in_flight).toBe(false);
    });

    it('stays open while the session waits on an answer', () => {
      const next = applyUpdate(empty(), {
        chat_appended: { key: SLOT, msg: state('requires_action') },
      });

      expect(next.header.turn_in_flight).toBe(true);
    });

    it('closes on a turn error, which is the only one of the three the core emits', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, { turn_error: { key: SLOT, message: 'stdin write failed' } });

      expect(next.header.turn_in_flight).toBe(false);
    });
  });

  describe('the header', () => {
    it('replaces the context usage a context_usage_snapshot carries', () => {
      const next = applyUpdate(empty(), {
        context_usage_snapshot: { key: SLOT, percentage: 42, max_tokens: 200_000 },
      });

      expect(next.header.context.percent).toBe(42);
      expect(next.header.context.max_tokens).toBe(200_000);
    });

    it('leaves the usage alone for a payload without the field it reads', () => {
      const held = applyUpdate(empty(), {
        context_usage_snapshot: { key: SLOT, percentage: 42, max_tokens: 200_000 },
      });
      // The record calls this `percent` and the update calls it `percentage` -
      // a reducer reading the record's name finds nothing on the wire.
      const next = applyUpdate(held, { context_usage_snapshot: { key: SLOT, percent: 7 } });

      expect(next.header.context.percent).toBe(42);
      expect(next.header.context.max_tokens).toBe(200_000);
    });

    it('reads an absent percentage as absent rather than as the last one', () => {
      const held = applyUpdate(empty(), {
        context_usage_snapshot: { key: SLOT, percentage: 42, max_tokens: 200_000 },
      });
      const next = applyUpdate(held, {
        context_usage_snapshot: { key: SLOT, percentage: null, max_tokens: null },
      });

      expect(next.header.context.percent).toBeNull();
    });
  });

  describe('the mcp servers', () => {
    it('replaces the set a mcp_snapshot carries', () => {
      const next = applyUpdate(empty(), {
        mcp_snapshot: {
          key: SLOT,
          servers: [{ name: 'forge', status: 'connected', tools: [] }],
          error: null,
        },
      });

      expect(next.mcp?.servers.map((server) => server.name)).toEqual(['forge']);
      expect(next.mcp?.servers[0]?.status).toBe('connected');
    });

    it('carries the failure beside an empty set', () => {
      const next = applyUpdate(empty(), {
        mcp_snapshot: { key: SLOT, servers: [], error: 'the bridge did not answer' },
      });

      expect(next.mcp?.servers).toEqual([]);
      expect(next.mcp?.error).toBe('the bridge did not answer');
    });
  });

  describe('the parked prompt', () => {
    it('parks the permission a permission_request carries', () => {
      const next = applyUpdate(empty(), {
        permission_request: {
          key: SLOT,
          tool_id: 'tool-1',
          request: { tool_call: { tool_call_id: 'tool-1', title: 'Bash' }, options: [] },
        },
      });

      expect(next.pending_ask).toEqual({
        kind: 'permission',
        request: { tool_call: { tool_call_id: 'tool-1', title: 'Bash' }, options: [] },
      });
    });

    it('drops it when that interaction is resolved', () => {
      const held = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'tool-1',
          request: { tool_call: { tool_call_id: 'tool-1' }, prompt: { question: 'which?' } },
        },
      });
      const next = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'tool-1' },
      });

      expect(next.pending_ask).toBeNull();
    });

    it('keeps a prompt another interaction resolved', () => {
      const held = applyUpdate(empty(), {
        permission_request: {
          key: SLOT,
          tool_id: 'tool-1',
          request: { tool_call: { tool_call_id: 'tool-1' }, options: [] },
        },
      });
      const next = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'tool-2' },
      });

      expect(next).toBe(held);
    });

    it('parks a held slack draft, and drops the one that expired', () => {
      const draft = { id: 'd1', workspace: 'Trust Machines', text: 'hello' };
      const held = applyUpdate(empty(), { slack_post_pending: { key: SLOT, draft } });
      expect(held.pending_ask).toEqual({ kind: 'slack_draft', request: draft });

      expect(
        applyUpdate(held, { slack_draft_expired: { key: SLOT, id: 'd1' } }).pending_ask,
      ).toBeNull();
      expect(applyUpdate(held, { slack_draft_expired: { key: SLOT, id: 'd2' } })).toBe(held);
    });
  });

  describe('the composer', () => {
    it('runs a take from its first reading to the notice it leaves', () => {
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      expect(started.composer.take).toMatchObject({ phase: 'recording', floor_db: -50 });

      const heard = applyUpdate(started, { dictate_level: { key: SLOT, peak_db: -20 } });
      // The meter draws fractions of the take's own range, computed where the
      // server computes them: the reading, its own floor, and the ceiling.
      expect(take(heard)['levels']).toEqual([0.6]);
      expect(take(heard)['peak_db']).toBe(-20);

      const transcribing = applyUpdate(heard, { dictate_transcribing: { key: SLOT } });
      expect(take(transcribing)['phase']).toBe('transcribing');

      const progress = applyUpdate(transcribing, {
        dictate_progress: { key: SLOT, generation: 1, done: 2, total: 3 },
      });
      expect(take(progress)['progress']).toEqual([2, 3]);

      const ended = applyUpdate(progress, {
        dictate_ended: {
          key: SLOT,
          outcome: { landed: { text: 'hello', truncated: false } },
          generation: 1,
        },
      });
      expect(ended.composer.take).toBeNull();
      expect(ended.composer.notice).toEqual({ kind: 'landed', text: 'hello', truncated: false });
    });

    it('drops a reading that belongs to no take', () => {
      const held = empty();
      expect(applyUpdate(held, { dictate_level: { key: SLOT, peak_db: -20 } })).toBe(held);
    });

    it('keeps a stalled take alive for its own generation only', () => {
      const first = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const second = applyUpdate(first, {
        dictate_started: { key: SLOT, floor_db: -40, generation: 2 },
      });
      const stale = applyUpdate(second, {
        dictate_progress: { key: SLOT, generation: 1, done: 9, total: 9 },
      });

      expect(take(stale)['progress']).toEqual([0, null]);
    });

    it('words the notice a take with nothing to insert leaves', () => {
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const ended = applyUpdate(started, {
        dictate_ended: { key: SLOT, outcome: 'empty', generation: 1 },
      });

      expect(ended.composer.notice).toEqual({
        kind: 'line',
        tone: 'q',
        text: 'that was all filler \u{b7} nothing to insert',
      });
    });

    it('clears the take and leaves nothing behind for an abandoned one', () => {
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const ended = applyUpdate(started, {
        dictate_ended: { key: SLOT, outcome: 'cancelled', generation: 1 },
      });

      expect(ended.composer.take).toBeNull();
      expect(ended.composer.notice).toBeNull();
    });

    it('raises the sign-in hint an auth_required carries', () => {
      const next = applyUpdate(empty(), {
        auth_required: {
          key: SLOT,
          method_name: 'claude.ai',
          method_description: 'Run `claude auth login`',
        },
      });

      expect(next.composer.sign_in).toEqual({
        method_name: 'claude.ai',
        method_description: 'Run `claude auth login`',
      });
    });

    it('compacts on the status frame and stops on the null that clears it', () => {
      const compacting = applyUpdate(empty(), {
        chat_appended: {
          key: SLOT,
          msg: { type: 'system', subtype: 'status', status: 'compacting' },
        },
      });
      expect(compacting.composer.compacting).toBe(true);

      const settled = applyUpdate(compacting, {
        chat_appended: { key: SLOT, msg: { type: 'system', subtype: 'status', status: null } },
      });
      expect(settled.composer.compacting).toBe(false);
    });
  });

  describe('what it does not handle', () => {
    it('leaves the record alone for a variant it does not handle', () => {
      const held = empty();
      expect(applyUpdate(held, { catalog_loaded: null })).toBe(held);
    });

    it('leaves the record alone for a payload naming a field it does not read', () => {
      const held = empty();
      expect(applyUpdate(held, { mcp_snapshot: { key: SLOT, servers: 'not a list' } })).toBe(held);
    });

    it('names every field only a read can move', () => {
      const held = empty();
      expect([...UNFED].every((field) => field in held)).toBe(true);
    });
  });
});

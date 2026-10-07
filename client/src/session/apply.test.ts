import { describe, expect, it } from 'vitest';

import { applyUpdate, HANDLERS, IGNORED, REPLACES } from './apply';
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

/**
 * One held ask as its own key: the call and the round, which is what tells a
 * queue apart. A permission carries no round and reads as `id:undefined`,
 * which no question test shares.
 */
function roundOf(ask: unknown): string {
  const held = ask as Record<string, unknown>;
  const request = (held['request'] ?? {}) as Record<string, unknown>;
  const call = (request['tool_call'] ?? {}) as Record<string, unknown>;
  return `${String(call['tool_call_id'])}:${String(request['question_index'])}`;
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

/**
 * The frame a turn opens with.
 *
 * The CLI re-fires one at the head of every turn, and it is where the model a
 * `/model` switch moved to, and the mode a `/mode` moved to, reach a reader.
 * The shape is off the live captures: `model` is the CLI's own spelling, the
 * `[1m]` marker included, and the mode is camelCase.
 */
function initFrame(fields: Record<string, unknown> = {}): Record<string, unknown> {
  return { type: 'system', subtype: 'init', session_id: 's', ...fields };
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
  describe('a parked prompt', () => {
    /** A round of a question batch: one tool call id, an advancing index. */
    const asked = (index: number) =>
      ({
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: {
            tool_call: { tool_call_id: 'toolu_q' },
            prompt: { questions: [] },
            question_index: index,
            total_questions: 2,
          },
        },
      }) as const;

    it('keeps an ask a resolution does not name by its round', () => {
      // A batch reuses one tool call id and advances the question index, and
      // the next round's request can land BEFORE the previous round's
      // resolution - so a clear keyed on the id alone dropped the ask that
      // had just parked, and every round after the first lost its opening
      // question (Ved's live find; #1717). The frame names the round it ends.
      let held = empty();
      held = applyUpdate(held, asked(1));
      expect(held.pending_asks.map(roundOf), "the round's ask parks").toEqual(['toolu_q:1']);

      held = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'toolu_q', question_index: 0 },
      });
      expect(
        held.pending_asks.map(roundOf),
        'a resolution for an earlier round leaves the parked ask standing',
      ).toEqual(['toolu_q:1']);

      held = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'toolu_q', question_index: 1 },
      });
      expect(held.pending_asks, 'and its own round settles it').toEqual([]);
    });

    it('keeps both rounds of one call, oldest first', () => {
      // One tool call carries a whole batch and advances the round, so its
      // rounds share a tool id and the round is what tells them apart: a key
      // that dropped it would fold round 1 into round 0 and draw one question
      // twice.
      let held = empty();
      held = applyUpdate(held, asked(0));
      held = applyUpdate(held, asked(1));

      expect(held.pending_asks.map(roundOf), 'both rounds are held, oldest first').toEqual([
        'toolu_q:0',
        'toolu_q:1',
      ]);
    });

    it('keeps a second parallel ask, and draws the front of the queue', () => {
      // **The real loss, measured by the review's fresh-read axis (forge's
      // own grilling session, 2026-10-04)**: two AskUserQuestion calls in
      // ONE assistant message run in PARALLEL with different tool ids. The
      // record's single slot dropped A the moment B parked, resolution(A)
      // then id-mismatched and was ignored, and A's next round replaced B
      // before B ever drew. The record now holds every ask it is told to
      // park, oldest first, and a resolution takes its own out by the pair.
      const round = (id: string, index: number) =>
        ({
          question_request: {
            key: SLOT,
            tool_id: id,
            request: {
              tool_call: { tool_call_id: id },
              prompt: { questions: [] },
              question_index: index,
              total_questions: 2,
            },
          },
        }) as const;

      let held = empty();
      held = applyUpdate(held, round('toolu_a', 0));
      held = applyUpdate(held, round('toolu_b', 0));
      held = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'toolu_a', question_index: 0 },
      });
      held = applyUpdate(held, round('toolu_a', 1));

      expect(JSON.stringify(held.pending_asks), "B's ask survives A's next round").toContain(
        'toolu_b',
      );
      expect(
        held.pending_asks.map(roundOf),
        "B leads the queue, and A's next round stands behind it",
      ).toEqual(['toolu_b:0', 'toolu_a:1']);
    });

    it('clears by the tool id alone for a frame that names no round', () => {
      // An older core sends no round - the field is absent, not null - and
      // the id is all it can mean there.
      let held = empty();
      held = applyUpdate(held, asked(2));
      held = applyUpdate(held, {
        pending_interaction_resolved: { key: SLOT, tool_id: 'toolu_q' },
      });
      expect(held.pending_asks, 'the id still settles it against an older core').toEqual([]);
    });
  });

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
    // The rising edge is the frame a turn OPENS with, because the frame a
    // reader might expect to carry it - `system/session_state_changed` - is
    // emitted only behind an environment variable forge never sets, and
    // appears in none of the committed live captures. The CLI re-fires `init`
    // at the head of every turn, which is the frame that does arrive.
    it('opens on the frame a turn begins with', () => {
      const next = applyUpdate(empty(), {
        chat_appended: { key: SLOT, msg: initFrame({ model: 'claude-opus-5' }) },
      });

      expect(next.header.turn_in_flight, 'a turn that began never read as running').toBe(true);
    });

    it('closes on a result frame', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, { chat_appended: { key: SLOT, msg: result() } });

      expect(next.header.turn_in_flight).toBe(false);
    });

    it('closes on a result that failed, which is a turn that ended too', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, { chat_appended: { key: SLOT, msg: result(true) } });

      expect(next.header.turn_in_flight).toBe(false);
    });

    // `Message::Error` is the CLI's last-gasp transport failure, after which
    // no result follows - the rule the core's own turn-commit marker clears
    // on (`crates/forge-workspace/src/session_task.rs`).
    it('closes on the error frame the CLI gives up with', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, {
        chat_appended: { key: SLOT, msg: { type: 'error', error: 'read loop died' } },
      });

      expect(next.header.turn_in_flight, 'a turn the CLI gave up on stayed in flight').toBe(false);
    });

    it('closes on a turn error, which is the only one of the three the core emits', () => {
      const held = { ...empty(), header: { ...empty().header, turn_in_flight: true } };
      const next = applyUpdate(held, { turn_error: { key: SLOT, message: 'stdin write failed' } });

      expect(next.header.turn_in_flight).toBe(false);
    });
  });

  describe('the pushed sets', () => {
    it('replaces the working tree, its PR and its closing issues from one frame', () => {
      const next = applyUpdate(empty(), {
        work_changed: {
          key: SLOT,
          work: { branch: 'main', changed: 2, gate: 'in_repo' },
          git: {
            default_branch: 'main',
            worktree: {
              files: [{ path: 'a.rs', added: 1, removed: 0, status: 'modified' }],
            },
            ahead: { commit_count: 1, commits: [{ sha: 'a1b2c3d', subject: 'the commit' }] },
          },
          pr: { number: 1249, url: 'https://example.test/pull/1249', draft: true },
          closes: [{ number: 1215, url: 'https://example.test/1215' }],
        },
      });

      expect(next.work).toEqual({ branch: 'main', changed: 2, gate: 'in_repo' });
      // A frame's stats block without totals falls back to the file list's
      // own length and zeroes, and a layer the frame does not carry states
      // nothing rather than guessing.
      expect(next.git, 'the tree behind the row rides the same frame').toEqual({
        defaultBranch: 'main',
        worktree: {
          files: [{ path: 'a.rs', added: 1, removed: 0, status: 'modified' }],
          totalFiles: 1,
          totalAdded: 0,
          totalRemoved: 0,
        },
        ahead: {
          count: 1,
          commits: [{ sha: 'a1b2c3d', subject: 'the commit', stats: null, time: 0 }],
          stats: null,
        },
      });
      expect(next.pr).toEqual({
        number: 1249,
        url: 'https://example.test/pull/1249',
        draft: true,
      });
      expect(next.closes).toEqual([{ number: 1215, url: 'https://example.test/1215' }]);
    });

    it('replaces the monitor set whole', () => {
      const next = applyUpdate(empty(), {
        monitors_changed: {
          key: SLOT,
          monitors: [
            {
              tool_use_id: 'tu-1',
              task_id: 't-1',
              description: 'ci-watch',
              command: 'gh run watch 1',
              persistent: true,
              timeout_ms: 0,
              status: 'running',
              output_file: null,
              ended_at: null,
            },
          ],
        },
      });

      expect(next.monitors).toHaveLength(1);
      expect(next.monitors[0]?.description).toBe('ci-watch');
    });

    it('reads the registry off the payload key the frame carries', () => {
      // The frame names the set `tasks`; the record's field is
      // `background_tasks`, and reading the payload by the record's name is
      // the mistake that leaves the field empty forever.
      const next = applyUpdate(empty(), {
        background_tasks_changed: {
          key: SLOT,
          tasks: [
            {
              task_id: 't-1',
              task_type: 'local_bash',
              description: 'gh run watch',
              command: 'gh run watch 1',
            },
          ],
        },
      });

      expect(next.background_tasks).toHaveLength(1);
      expect(next.background_tasks[0]).toMatchObject({ task_id: 't-1' });
    });

    it('clears a set the frame says is empty', () => {
      const held = applyUpdate(empty(), {
        background_tasks_changed: {
          key: SLOT,
          tasks: [{ task_id: 't-1', task_type: 'local_bash', description: 'x', command: null }],
        },
      });
      const next = applyUpdate(held, { background_tasks_changed: { key: SLOT, tasks: [] } });

      expect(next.background_tasks, 'an empty set is the frame saying nothing is running').toEqual(
        [],
      );
    });

    it('replaces each catalogue whole, off the payload key the frame carries', () => {
      const commands = applyUpdate(empty(), {
        slash_commands_changed: { key: SLOT, commands: [{ name: 'compact' }] },
      });
      const agents = applyUpdate(commands, {
        subagents_changed: { key: SLOT, subagents: [{ name: 'reviewer' }] },
      });

      expect(agents.slash_commands).toEqual([{ name: 'compact' }]);
      expect(agents.subagents).toEqual([{ name: 'reviewer' }]);
    });

    it('carries the dispatch flag the frame names', () => {
      const next = applyUpdate(empty(), {
        dispatches_changed: { key: SLOT, has_dispatches: true },
      });

      expect(next.has_dispatches).toBe(true);
    });

    it('carries the joined instances the frame names, ending stamp and all', () => {
      const next = applyUpdate(empty(), {
        subagent_cards_changed: {
          key: SLOT,
          cards: [
            {
              name: 'map the calls',
              dispatch_id: 'tu-sub',
              agent_type: 'general-purpose',
              running: false,
              failed: false,
              backgrounded: true,
              ended_at_ms: 1_750_000_000_123,
              calls: 2,
              tail: [{ name: 'Read', title: 'Read src/lib.rs', status: 'completed' }],
              usage: { total_tokens: 9714, tool_uses: 1, duration_ms: 2716 },
            },
          ],
        },
      });

      expect(next.subagent_instances).toEqual([
        {
          name: 'map the calls',
          dispatch_id: 'tu-sub',
          agent_type: 'general-purpose',
          running: false,
          failed: false,
          backgrounded: true,
          // The wire stamps Unix milliseconds; the record carries WireTime.
          ended_at: { secs_since_epoch: 1_750_000_000, nanos_since_epoch: 123_000_000 },
          calls: 2,
          tail: [{ name: 'Read', title: 'Read src/lib.rs', status: 'completed' }],
          usage: { total_tokens: 9714, tool_uses: 1, duration_ms: 2716 },
        },
      ]);
    });

    it('carries the file index the frame names', () => {
      const index = { entries: { 'src/main.rs': { rel_path: 'src/main.rs', depth: 1 } } };
      const next = applyUpdate(empty(), { file_index_changed: { key: SLOT, index } });

      expect(next.file_index, 'the walk the frame carried never reached the record').toEqual(index);
    });

    it('leaves the index alone for a frame that carries none', () => {
      const held = applyUpdate(empty(), {
        file_index_changed: { key: SLOT, index: { entries: { 'a.ts': {} } } },
      });
      const next = applyUpdate(held, { file_index_changed: { key: SLOT } });

      expect(next).toBe(held);
    });
  });

  describe('the header the frames move', () => {
    it('takes the mode and the effort a hook observation carries', () => {
      const next = applyUpdate(empty(), {
        hook_observation: {
          key: SLOT,
          tool_use_id: 'tool-1',
          permission_mode: 'plan',
          effort: 'max',
          agent_id: null,
          agent_type: null,
        },
      });

      expect(next.header.permission_mode).toBe('plan');
      expect(next.header.effort).toBe('max');
    });

    it('leaves the header alone for a hook observation that names neither', () => {
      const held = applyUpdate(empty(), {
        hook_observation: { key: SLOT, permission_mode: 'plan', effort: 'max' },
      });
      const next = applyUpdate(held, { hook_observation: { key: SLOT, tool_use_id: null } });

      expect(next, 'a hook with nothing to say narrowed the header').toBe(held);
    });

    it('takes the mode and the model a turn opens with', () => {
      const next = applyUpdate(empty(), {
        chat_appended: {
          key: SLOT,
          msg: initFrame({ model: 'claude-opus-5[1m]', permissionMode: 'acceptEdits' }),
        },
      });

      expect(next.header.permission_mode).toBe('acceptEdits');
      expect(next.header.model?.resolved_id).toBe('claude-opus-5[1m]');
    });

    it('keeps the name a connect resolved when the frame names the same model', () => {
      const held = {
        ...empty(),
        header: {
          ...empty().header,
          model: { resolved_id: 'claude-opus-5', display_name_long: 'Opus 5' },
        },
      };

      const next = applyUpdate(held, {
        chat_appended: { key: SLOT, msg: initFrame({ model: 'claude-opus-5' }) },
      });

      expect(next.header.model, 'a frame naming the model already held renamed it').toEqual({
        resolved_id: 'claude-opus-5',
        display_name_long: 'Opus 5',
      });
    });

    it('names a switched model from the catalogue the record already holds', () => {
      const held = {
        ...empty(),
        header: {
          ...empty().header,
          model: { resolved_id: 'claude-opus-5', display_name_long: 'Opus 5' },
          available_models: [{ id: 'claude-sonnet-4-6', display_name: 'Sonnet 4.6' }],
        },
      };

      const next = applyUpdate(held, {
        chat_appended: { key: SLOT, msg: initFrame({ model: 'claude-sonnet-4-6' }) },
      });

      expect(next.header.model).toEqual({
        resolved_id: 'claude-sonnet-4-6',
        display_name_long: 'Sonnet 4.6',
      });
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

      // The record itself, not just its values: a payload carrying neither half
      // is not a read of the usage, so there is nothing to publish - and a page
      // redrawing for a frame that said nothing is what the rest of this file
      // returns early to avoid.
      expect(next, 'a payload naming a field nothing sends republished the record').toBe(held);
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

      expect(next.pending_asks).toEqual([
        {
          kind: 'permission',
          request: { tool_call: { tool_call_id: 'tool-1', title: 'Bash' }, options: [] },
        },
      ]);
    });

    it('keeps one ask when the same request arrives twice', () => {
      // The single slot was idempotent under a re-delivered frame, and the
      // queue has to stay idempotent the same way: the read a seat takes is
      // the record as the socket had it, and a frame the read already carried
      // arrives once more beside it.
      const held = applyUpdate(empty(), {
        permission_request: {
          key: SLOT,
          tool_id: 'tool-1',
          request: { tool_call: { tool_call_id: 'tool-1' }, options: [] },
        },
      });
      const next = applyUpdate(held, {
        permission_request: {
          key: SLOT,
          tool_id: 'tool-1',
          request: { tool_call: { tool_call_id: 'tool-1' }, options: [] },
        },
      });

      expect(next.pending_asks, 'a repeat is not a second ask').toHaveLength(1);
      expect(next, 'and it leaves the record as it was').toBe(held);
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

      expect(next.pending_asks).toEqual([]);
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

    it('lands a draft ahead of the asks already waiting', () => {
      // The rule every hop keeps: a draft leads the queue, then arrival
      // order. The draft's registry carries no arrival order to offer, and
      // the single read always answered a draft first - so a fold that
      // appended it would let a re-read flip the front against the fold.
      const kinds = (held: SessionRecord): unknown[] =>
        held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

      const asked = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: { tool_call: { tool_call_id: 'toolu_q' }, prompt: { question: 'which?' } },
        },
      });
      const draft = { id: 'd1', workspace: 'Trust Machines', text: 'hello' };
      const parked = applyUpdate(asked, { slack_post_pending: { key: SLOT, draft } });

      expect(kinds(parked), 'the draft leads the question that was already waiting').toEqual([
        'slack_draft',
        'question',
      ]);
      expect(
        kinds(applyUpdate(parked, { slack_draft_resolved: { key: SLOT, id: 'd1' } })),
        'and its resolution falls back to the question',
      ).toEqual(['question']);
    });

    it('parks a held slack draft, and drops the one the core resolved', () => {
      const draft = { id: 'd1', workspace: 'Trust Machines', text: 'hello' };
      const held = applyUpdate(empty(), { slack_post_pending: { key: SLOT, draft } });
      expect(held.pending_asks).toEqual([{ kind: 'slack_draft', request: draft }]);

      expect(
        applyUpdate(held, { slack_draft_resolved: { key: SLOT, id: 'd1' } }).pending_asks,
      ).toEqual([]);
      expect(applyUpdate(held, { slack_draft_resolved: { key: SLOT, id: 'd2' } })).toBe(held);
    });

    it('parks a browser hand-off, ahead of the asks already waiting, the way the read orders it', () => {
      const kinds = (held: SessionRecord): unknown[] =>
        held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

      const asked = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: { tool_call: { tool_call_id: 'toolu_q' }, prompt: { question: 'which?' } },
        },
      });
      const handoff = { id: 'h1', reason: 'solve the CAPTCHA', context: 'job-hunt' };
      const parked = applyUpdate(asked, { browser_hand_off_pending: { key: SLOT, handoff } });

      expect(kinds(parked), 'the hand-off leads the question that was already waiting').toEqual([
        'browser_hand_off',
        'question',
      ]);
      expect(
        kinds(applyUpdate(parked, { browser_hand_off_resolved: { key: SLOT, id: 'h1' } })),
        'and its resolution falls back to the question',
      ).toEqual(['question']);
      expect(
        applyUpdate(parked, { browser_hand_off_resolved: { key: SLOT, id: 'h2' } }),
        'a resolution naming another hand-off leaves this one parked',
      ).toBe(parked);
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

    it("draws the core's own refusal line for a take the server refused", () => {
      // The client starts the take locally and the server refuses it - a
      // second take on a busy seat, or on another seat of this connection -
      // so the line is the core's wording, passed through.
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const ended = applyUpdate(started, {
        dictate_ended: {
          key: SLOT,
          outcome: {
            refused: {
              message:
                'another client is already dictating on this session \u{b7} dictation did not start',
            },
          },
          generation: 0,
        },
      });

      expect(ended.composer.take).toBeNull();
      expect(ended.composer.notice).toEqual({
        kind: 'line',
        tone: 'bad',
        text: 'another client is already dictating on this session \u{b7} dictation did not start',
      });
    });

    it('keeps the take for a progress report that carries no counts', () => {
      const held = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });

      const next = applyUpdate(held, { dictate_progress: { key: SLOT, generation: 1 } });

      expect(next, 'a report with nothing in it narrowed the take').toBe(held);
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

    it('leaves a held sign-in alone for an auth_required that names no method', () => {
      const held = applyUpdate(empty(), {
        auth_required: {
          key: SLOT,
          method_name: 'claude.ai',
          method_description: 'run `claude auth login`',
        },
      });

      const next = applyUpdate(held, { auth_required: { key: SLOT } });

      expect(next, 'a payload with nothing in it overwrote the sign-in').toBe(held);
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

  /**
   * The override set a `/dictate` edit leaves behind. The core echoes the whole
   * set on an update of its own after every set and reset - so it is folded,
   * the way the terminal's own arm folds it, rather than left to a read.
   */
  describe('the dictate override echo', () => {
    it("leaves the record alone: the axes are this client's own now", () => {
      // The core still echoes `dictate_overrides` for the terminal's own
      // `/dictate` overlay, and this client holds its axes itself - so the
      // echo is an update with nothing here to write it to, and the record
      // stands rather than gaining a field nothing reads.
      const held = empty();
      const next = applyUpdate(held, {
        dictate_overrides: {
          key: SLOT,
          overrides: { styling: 'formal', structure: 'lists', context: null },
        },
      });
      expect(next, "the override echo must not touch this side's record").toBe(held);
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
  });

  describe('the queue', () => {
    const queued = { key: SLOT, uuid: 'u7', source: 'cron', text: 'nightly sweep' };

    it('adds a row from the update that carries its words and sender', () => {
      const next = applyUpdate(empty(), { prompt_queued: queued });

      expect(next.queue).toEqual([{ uuid: 'u7', source: 'cron', text: 'nightly sweep' }]);
    });

    it('keeps one row when the same id arrives twice', () => {
      const once = applyUpdate(empty(), { prompt_queued: queued });
      const twice = applyUpdate(once, { prompt_queued: queued });

      expect(twice.queue).toHaveLength(1);
      expect(twice, 'a duplicate leaves the record as it was').toBe(once);
    });

    it('drops the row on a state the CLI settled, and keeps it while queued', () => {
      const held = applyUpdate(empty(), { prompt_queued: queued });

      const still = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'queued' },
      });
      expect(still.queue, 'a second queued frame is not a delivery').toHaveLength(1);

      const delivered = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'started' },
      });
      expect(delivered.queue, 'started is the CLI taking the prompt').toEqual([]);
    });

    it('says so when a row left by discard or refusal, and not when it was taken', () => {
      const held = applyUpdate(empty(), { prompt_queued: queued });

      const delivered = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'started' },
      });
      expect(delivered.queue_ended, 'being taken is the row doing its job').toBeNull();

      const discarded = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'discarded' },
      });
      expect(discarded.queue, 'the row leaves either way').toEqual([]);
      expect(discarded.queue_ended).toEqual({ text: 'nightly sweep', state: 'discarded' });

      const refused = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'refused' },
      });
      expect(refused.queue_ended).toEqual({ text: 'nightly sweep', state: 'refused' });
    });

    it('clears the last ending when the next prompt is queued', () => {
      const held = applyUpdate(empty(), { prompt_queued: queued });
      const ended = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'discarded' },
      });

      const next = applyUpdate(ended, {
        prompt_queued: { key: SLOT, uuid: 'u8', source: 'you', text: 'the next thing' },
      });

      expect(next.queue_ended, 'the ending goes with the queue that moved on').toBeNull();
    });

    it('keeps the row on a state this build cannot name', () => {
      const held = applyUpdate(empty(), { prompt_queued: queued });

      const next = applyUpdate(held, {
        prompt_lifecycle: { key: SLOT, uuid: 'u7', state: 'preempted' },
      });

      expect(next.queue, 'a word the CLI adds later must not empty the pile').toHaveLength(1);
      expect(next).toBe(held);
    });

    it('drops the row when a cancel is confirmed, and keeps it when it was too late', () => {
      const held = applyUpdate(empty(), { prompt_queued: queued });

      const late = applyUpdate(held, {
        prompt_cancel_resolved: { key: SLOT, uuid: 'u7', cancelled: false },
      });
      expect(late.queue, 'already taken is not dropped').toHaveLength(1);

      const dropped = applyUpdate(held, {
        prompt_cancel_resolved: { key: SLOT, uuid: 'u7', cancelled: true },
      });
      expect(dropped.queue).toEqual([]);
    });

    it("clears a dead CLI's rows, where no per-row frame ever will", () => {
      // The refuting sequence, on the seat kind it strands: a NON-LEAD seat,
      // a queue the read carried, and the death event - the lead seats
      // self-heal through the auto-respawn's REPLACES, workers do not, and
      // the handler is label-blind. The queue died with the process and the
      // core clears its own pile on the same event; without the client doing
      // the same the cards stand until something reads the seat again, which
      // for a dead worker is its next spawn - and a row that vanishes with
      // no word reads as one that was delivered.
      const worker = sessionFrom({
        slot: { org: 'Busytools', project: 'forge', label: 'w1' },
        state: { scan_cwd: '/tmp' },
      });
      const held = applyUpdate(worker, { prompt_queued: queued });
      const next = applyUpdate(held, {
        connection_failed: {
          key: { org: 'Busytools', project: 'forge', label: 'w1' },
          message: 'the process exited',
          fatal: false,
        },
      });

      expect(next.queue, 'the rows go with the process, on the death event').toEqual([]);
      expect(next.queue_ended, 'one dim line where the cards were').toEqual({
        text: 'nightly sweep',
        state: 'discarded',
      });

      const bare = empty();
      const nothing = applyUpdate(bare, {
        connection_failed: { key: SLOT, message: 'the process exited', fatal: false },
      });
      expect(nothing, 'a seat with no rows is left as it was').toBe(bare);
    });
  });
});

/**
 * Every variant `SessionUpdate` carries - 73 of them - read off the enum in
 * `crates/forge-workspace/src/protocol.rs` and held here as a set rather than
 * in any order: the assertions below filter over it, and the test beside the
 * enum reads it back to check the two carry the same names.
 *
 * **The copy is the point.** Nothing compiles the link between the Rust enum
 * and this table, so a variant added there arrives here as a name nothing
 * knows - and a name nothing knows is a lookup that misses, which is a page
 * that stops following its seat with nothing to report. The test beside the
 * enum (`every_session_update_variant_is_classified_for_the_client`) reads
 * both ends and fails on a name this list does not carry, so the two move
 * together or one of them goes red.
 */
const EVERY_VARIANT = [
  'spawning',
  'connected',
  'history_replayed',
  'session_replaced',
  'connection_failed',
  'auth_required',
  'slash_command_error',
  'notice',
  'runtime_reload_completed',
  'runtime_reload_failed',
  'set_mode_failed',
  'set_model_failed',
  'permission_request',
  'question_request',
  'pending_interaction_resolved',
  'mcp_operation_error',
  'turn_complete',
  'turn_cancelled',
  'turn_error',
  'chat_appended',
  'hook_observation',
  'status_snapshot',
  'forge_account_identity',
  'dictate_overrides',
  'dictate_device_pin',
  'oauth_credentials_snapshot',
  'context_usage_snapshot',
  'mcp_snapshot',
  'monitors_changed',
  'background_tasks_changed',
  'work_changed',
  'tasks_changed',
  'cron_schedules_changed',
  'connector_subscriptions_changed',
  'slash_commands_changed',
  'subagents_changed',
  'dispatches_changed',
  'subagent_cards_changed',
  'file_index_changed',
  'processes_changed',
  'sessions_listed',
  'service_status',
  'catalog_loaded',
  'cli_version_changed',
  'accounts_changed',
  'plugins_inventory_updated',
  'plugins_inventory_refresh_failed',
  'plugins_cli_action_succeeded',
  'plugins_cli_action_failed',
  'plugins_update_run_progress',
  'plugins_update_run_finished',
  'plugins_rollback_succeeded',
  'plugins_rollback_failed',
  'worker_status_changed',
  'peer_envelope_appended',
  'gotify_notification_appended',
  'cron_prompt_appended',
  'slack_message_appended',
  'slack_post_pending',
  'slack_draft_resolved',
  'browser_hand_off_pending',
  'browser_hand_off_resolved',
  'prompt_queued_while_busy',
  'prompt_queued',
  'prompt_lifecycle',
  'prompt_cancel_resolved',
  'review_activity_notice',
  'dictate_availability',
  'dictate_started',
  'dictate_level',
  'dictate_transcribing',
  'dictate_progress',
  'dictate_ended',
  'fatal_error',
  'dictate_models_changed',
] as const;

/**
 * A variant this build has never heard of leaves the record exactly as it was.
 *
 * **The server ships ahead of the client**, so a frame from a newer core
 * arrives here before anything reads it: an update naming a variant this
 * build does not know has to be a no-op - the pane keeps what the last read
 * answered and its own frames have carried since - rather than a crash or a
 * field defaulted back. `HANDLERS`'s miss is what makes that true, and this
 * pins the arm rather than assuming it.
 */
it('leaves the record alone for a variant it does not know', () => {
  const held = empty();
  const next = applyUpdate(held, {
    a_variant_this_build_lacks: {
      key: SLOT,
      work: { branch: 'main', changed: 2, gate: 'in_repo' },
    },
  });

  expect(next, 'an unknown variant must not move what the page holds').toBe(held);
});

describe('the read', () => {
  it('holds the list when the read carries both, and a lone ask as a list of one', () => {
    // A newer core serves `pending_asks` beside the derived `pending_ask`,
    // and an older one serves only the single ask. Reading the singular
    // first would draw the derived front twice and drop what is behind it;
    // dropping the fallback would read an older core's held ask as nothing.
    const question = { kind: 'question', request: { tool_call: { tool_call_id: 'tu-a' } } };
    const permission = { kind: 'permission', request: { tool_call: { tool_call_id: 'tu-b' } } };
    const draft = { kind: 'slack_draft', request: { id: 'd1' } };
    const kinds = (held: SessionRecord): unknown[] =>
      held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

    const both = sessionFrom({
      slot: SLOT,
      state: { scan_cwd: '/tmp' },
      pending_asks: [question, permission],
      pending_ask: draft,
    });
    expect(kinds(both), 'the list wins, in the order the read answered').toEqual([
      'question',
      'permission',
    ]);

    const older = sessionFrom({ slot: SLOT, state: { scan_cwd: '/tmp' }, pending_ask: draft });
    expect(kinds(older), 'and a lone ask is a list of one').toEqual(['slack_draft']);
  });
});

describe('the variant list', () => {
  it('carries the whole census, so the two assertions below can be trusted', () => {
    // **The count, because `[].filter(f).toEqual([])` holds for every `f`.** A
    // list truncated by an edit retires the assertions below it for the names
    // it lost and reads green, so the length is asserted rather than floored:
    // raise it in the same edit that adds a variant, as the plan says.
    expect(
      EVERY_VARIANT.length,
      'the census no longer carries every variant the enum declares (75 of them): a truncated ' +
        'census leaves the assertions below checking only the names it still has',
    ).toBe(75);
  });

  it('classifies every variant the core can send', () => {
    const unclassified = EVERY_VARIANT.filter(
      (name) => !(name in HANDLERS) && !REPLACES.includes(name) && !IGNORED.includes(name),
    );

    expect(
      unclassified,
      'a variant nothing classifies reaches the page and is dropped without a word',
    ).toEqual([]);
  });

  it('classifies each variant once', () => {
    const twice = EVERY_VARIANT.filter(
      (name) =>
        (name in HANDLERS ? 1 : 0) +
          (REPLACES.includes(name) ? 1 : 0) +
          (IGNORED.includes(name) ? 1 : 0) >
        1,
    );

    expect(twice, 'a variant claimed by two lists is a decision nobody can read').toEqual([]);
  });
});

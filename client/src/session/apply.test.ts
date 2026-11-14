import { describe, expect, it } from 'vitest';

import { joinOfEvents } from '../chat/join';
import { outcomesFrom, outcomesOf } from '../chat/outcomes';
import { machineInForce } from '../composer/wire';
import { startPerfProbe, type PerfRecord } from '../perf';
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

/**
 * The frame built in the tests of the view this client replaced, which is what
 * the CLI prints on a result.
 */
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

/** A tool call, as the assistant frame that carries it. */
function toolUse(
  id: string,
  name: string,
  input: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    type: 'assistant',
    uuid: `a-${id}`,
    message: { role: 'assistant', content: [{ type: 'tool_use', id, name, input }] },
  };
}

/** A tool's own answer. */
function toolResult(id: string, content: string): Record<string, unknown> {
  return {
    type: 'user',
    uuid: `r-${id}`,
    message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content }] },
  };
}

/** A task frame of the CLI's own (`task_started`, `task_updated`, ...). */
function taskFrame(fields: Record<string, unknown>): Record<string, unknown> {
  return { type: 'system', ...fields };
}

/** Apply one frame as the live stream delivers it. */
function landed(held: SessionRecord, msg: unknown): SessionRecord {
  return applyUpdate(held, { chat_appended: { key: SLOT, msg } });
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
    /**
     * The join is materialised as frames land, so the strip never walks the
     * conversation. The whole walk is still the authority: a fresh read builds
     * it that way, and this is where the two must agree.
     */
    it('folds the task join the conversation states, as a whole walk of it would', () => {
      const frames = [
        prompt('go', 'p-0'),
        toolUse('call-launch', 'Bash'),
        toolResult('call-launch', 'Command running in background with ID: t-launch'),
        // **A dispatched sub-agent's frame is not this session's.** If the
        // guard goes, its failure invents a call the strip would draw.
        {
          ...taskFrame({
            subtype: 'task_notification',
            tool_use_id: 'call-sub',
            status: 'failed',
          }),
          parent_tool_use_id: 'call-parent',
        },
        // **Parallel blocks in one frame**, which real frames carry: a reader
        // that stopped at the first block would drop the launch below.
        {
          type: 'assistant',
          uuid: 'a-many',
          message: {
            role: 'assistant',
            content: [
              { type: 'tool_use', id: 'call-first', name: 'Read', input: { file_path: '/tmp/a' } },
              { type: 'tool_use', id: 'call-second', name: 'Bash', input: { command: 'sleep 9' } },
            ],
          },
        },
        {
          type: 'user',
          uuid: 'r-many',
          message: {
            role: 'user',
            content: [
              { type: 'tool_result', tool_use_id: 'call-first', content: 'a' },
              {
                type: 'tool_result',
                tool_use_id: 'call-second',
                content: 'Command running in background with ID: t-second',
              },
            ],
          },
        },
        // The next turn resolves the task through the launch above: the frame
        // names only the task, which is the link the join exists for.
        prompt('next', 'p-1'),
        taskFrame({
          subtype: 'task_updated',
          task_id: 't-launch',
          patch: { status: 'completed', end_time: 1_700 },
        }),
        // And the parallel block's task resolves through its own link.
        taskFrame({
          subtype: 'task_updated',
          task_id: 't-second',
          patch: { status: 'failed', end_time: 1_800 },
        }),
        toolUse('call-stop', 'TaskStop', { task_id: 't-launch' }),
        toolResult('call-stop', 'Successfully stopped task: t-launch'),
        taskFrame({ subtype: 'task_notification', tool_use_id: 'call-plain', status: 'failed' }),
      ];
      const whole = outcomesFrom([{ messages: frames }]);
      let held = empty();
      for (const frame of frames) held = landed(held, frame);
      expect(held.conversation.outcomes, 'the live fold drifted from the whole walk').toEqual(
        whole,
      );

      // The same frames, arriving as a read: a fresh seat seeds the same join.
      const read = sessionFrom({
        slot: SLOT,
        state: { scan_cwd: '/tmp' },
        conversation: { turns: [{ key: null, messages: frames }], compaction_count: 0 },
      });
      // **The read's stored facts are the whole input**: folding them back is
      // where a second walk of the frames could drift from what was kept.
      expect(
        outcomesOf(joinOfEvents(read.conversation.turns)),
        "the read's stored facts fold to something other than the whole walk",
      ).toEqual(whole);
      expect(read.conversation.outcomes, "the read's join drifted from the whole walk").toEqual(
        whole,
      );
    });

    /**
     * The record keeps what the join reads and drops the frame it came from.
     * A body kept "for a reader" here is the leak this cut exists to close.
     */
    it('keeps no frame it has folded: the facts stay and the bodies go', () => {
      // One body on the frame that opens the turn and one on a frame that
      // joins it, so a record that keeps either is caught.
      const opening = 'the-prompt-the-record-must-not-keep';
      const body = 'the-result-the-record-must-not-keep';
      let held = landed(empty(), prompt(opening, 'p-0'));
      held = landed(held, toolUse('call-launch', 'Bash'));
      const next = landed(
        held,
        toolResult('call-launch', `Command running in background with ID: t-launch ${body}`),
      );

      const kept = JSON.stringify(next.conversation);
      expect(kept, 'the record kept the body of the frame that opened its turn').not.toContain(
        opening,
      );
      expect(kept, 'the record kept the body of a frame that joined its turn').not.toContain(body);
      // The fact the frame stated is still there, so the absence above is the
      // body going and not the frame never having been read.
      expect(kept, 'the fold kept no fact from the frame it read').toContain('t-launch');
    });

    it('opens a turn for what a person said, and joins the answer to it', () => {
      const spoken = landed(empty(), prompt('hello'));
      const answered = landed(spoken, {
        type: 'assistant',
        uuid: 'u2',
        message: { role: 'assistant', content: [{ type: 'text', text: 'hi' }] },
      });

      expect(answered.conversation.turns).toHaveLength(1);
    });

    it('answers a second prompt with a turn of its own', () => {
      const first = landed(empty(), prompt('one'));
      const second = landed(first, prompt('two'));

      expect(second.conversation.turns).toHaveLength(2);
    });

    it('keeps a tool result and a system frame inside the turn the prompt opened', () => {
      let held = landed(empty(), prompt('one'));
      held = landed(held, toolResult('tu1', 'a\nb'));
      held = landed(held, { type: 'system', subtype: 'init', session_id: 's', tools: [] });

      expect(held.conversation.turns, 'a frame that ends nothing opened a turn').toHaveLength(1);
      expect(
        held.conversation.turns[0]?.events,
        'the tool result was dropped instead of kept in the turn',
      ).toContainEqual({ kind: 'result', id: 'tu1', launches: null, stopped: false });
    });

    /**
     * The record keeps the window a fresh read sends, so a long-lived seat
     * retains no more than a reload would hand it. The window is whole turns:
     * a trim never cuts a turn's facts in half.
     */
    it('keeps the newest whole turns a fresh read would send', () => {
      const opened = 25;
      let held = empty();
      for (let at = 0; at < opened; at += 1) {
        held = landed(held, prompt(`turn-${at}`, `u-${at}`));
      }

      expect(held.conversation.turns).toHaveLength(20);
    });

    it('keeps the compaction count when the window trims', () => {
      let held = applyUpdate(empty(), { chat_appended: { key: SLOT, msg: prompt('one', 'u-0') } });
      held = applyUpdate(held, {
        chat_appended: { key: SLOT, msg: { type: 'system', subtype: 'compact_boundary' } },
      });
      for (let at = 0; at < 25; at += 1) {
        held = applyUpdate(held, {
          chat_appended: { key: SLOT, msg: prompt(`turn-${at}`, `u-${at + 1}`) },
        });
      }

      expect(held.conversation.turns).toHaveLength(20);
      expect(held.conversation.compaction_count).toBe(1);
    });

    /**
     * The strip reads the record's materialised join, so the window's risk is
     * pinned here: an ending inside it still lands, one whose turn left is
     * gone - the same loss a reload takes. The whole walk over the surviving
     * frames stays the authority at both checkpoints, which is what makes the
     * trim's replay exact.
     */
    it('loses an ending only when its turn leaves the window, as a reload does', () => {
      const frames: unknown[] = [
        prompt('opening', 'p-0'),
        taskFrame({
          subtype: 'task_notification',
          tool_use_id: 'call-window',
          status: 'failed',
          patch: { status: 'failed', end_time: 1_793_700_000_000 },
        }),
        // The owner link is here too, and its ending lands in a turn that
        // survives: a trim that patched the join rather than replaying it
        // would keep the link and resolve a call the window no longer holds.
        taskFrame({ subtype: 'task_started', task_id: 't-gone', tool_use_id: 'call-gone' }),
      ];
      // Nineteen turns after it: the ending's turn is still inside the window,
      // while a link and a later ending land in turns that survive the trim.
      for (let turn = 1; turn <= 19; turn += 1) {
        frames.push(prompt(`turn-${turn}`, `p-${turn}`));
        if (turn === 1) {
          frames.push(
            taskFrame({ subtype: 'task_started', task_id: 't-late', tool_use_id: 'call-late' }),
          );
        }
        if (turn === 2) {
          frames.push(
            taskFrame({
              subtype: 'task_updated',
              task_id: 't-gone',
              patch: { status: 'failed', end_time: 999 },
            }),
          );
        }
        if (turn === 5) {
          frames.push(
            taskFrame({
              subtype: 'task_notification',
              tool_use_id: 'call-late',
              status: 'failed',
              patch: { end_time: 2_222 },
            }),
          );
        }
      }
      let held = empty();
      for (const frame of frames) held = landed(held, frame);

      expect(
        held.conversation.outcomes.calls.get('call-window')?.failed,
        "the newest ending did not reach the strip's join",
      ).toBe(true);
      expect(
        held.conversation.outcomes.calls.get('call-gone')?.failed,
        'a link inside the window did not resolve the ending after it',
      ).toBe(true);
      expect(
        held.conversation.turns[0]?.events.some(
          (event) => event.kind === 'ending' && event.call === 'call-window',
        ),
        'the window did not start at the opening turn before the trim',
      ).toBe(true);
      expect(
        held.conversation.outcomes,
        'the folded join drifted from a whole walk of the frames it holds',
      ).toEqual(outcomesFrom([{ messages: frames }]));

      // One more turn and the ending's turn is the first the window drops. The
      // surviving frames are the reference: what the window keeps, it keeps
      // exactly as a whole walk of them would.
      held = landed(held, prompt('turn-20', 'p-20'));
      expect(
        held.conversation.turns[0]?.events.some(
          (event) => event.kind === 'started' && event.task === 't-late',
        ),
        'the trim did not start its window at the oldest turn it keeps',
      ).toBe(true);
      expect(
        held.conversation.outcomes.calls.get('call-window'),
        'an ending older than the window outlived it',
      ).toBeUndefined();
      expect(
        held.conversation.outcomes.calls.get('call-gone'),
        'a link in the dropped turn still resolved its later ending',
      ).toBeUndefined();
      expect(
        held.conversation.outcomes.calls.get('call-late')?.failed,
        "a surviving turn's ending was lost with the dropped turn",
      ).toBe(true);
      expect(
        held.conversation.outcomes,
        'the replay of the surviving window drifted from a whole walk of it',
      ).toEqual(outcomesFrom([{ messages: frames.slice(3) }]));

      // A frame after the replay folds into the window it just replayed.
      const after = taskFrame({
        subtype: 'task_notification',
        tool_use_id: 'call-after',
        status: 'failed',
        patch: { end_time: 4_444 },
      });
      held = landed(held, after);
      expect(
        held.conversation.outcomes.calls.get('call-after')?.failed,
        'the frame after the trim did not reach the join',
      ).toBe(true);
      expect(
        held.conversation.outcomes,
        'a frame after the trim drifted from a whole walk of the new window',
      ).toEqual(outcomesFrom([{ messages: [...frames.slice(3), after] }]));
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
        base: null,
        baseError: null,
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

    /**
     * **A failed refresh is news about the section, and the record is where a
     * row reads it from.** The operation error is a live frame - the
     * mcp_status timeout - and a record that dropped it left the section
     * drawing the last good read with nothing saying so.
     */
    it('names a failed refresh while keeping the servers the last read gave', () => {
      const held = applyUpdate(empty(), {
        mcp_snapshot: {
          key: SLOT,
          servers: [{ name: 'forge', status: 'connected', tools: [] }],
          error: null,
        },
      });
      const next = applyUpdate(held, {
        mcp_operation_error: {
          key: SLOT,
          error: { server_name: null, operation: 'status', message: 'no response from the CLI' },
        },
      });

      expect(next.mcp_operation_error).toEqual({
        server_name: null,
        operation: 'status',
        message: 'no response from the CLI',
      });
      expect(
        next.mcp?.servers.map((server) => server.name),
        'the last good read stands',
      ).toEqual(['forge']);
    });

    /** A frame that names no words is nothing to draw, so the record stays as it was. */
    it('drops a failed refresh that names no words', () => {
      const next = applyUpdate(empty(), {
        mcp_operation_error: {
          key: SLOT,
          error: { server_name: null, operation: 'status', message: '' },
        },
      });

      expect(next.mcp_operation_error).toBeNull();
    });

    /** A read landing is the staleness ending: it supersedes any failed refresh. */
    it('clears the failed refresh once a read lands', () => {
      const failed = applyUpdate(empty(), {
        mcp_operation_error: {
          key: SLOT,
          error: { server_name: null, operation: 'status', message: 'no response from the CLI' },
        },
      });
      const next = applyUpdate(failed, {
        mcp_snapshot: { key: SLOT, servers: [], error: null },
      });

      expect(next.mcp_operation_error).toBeNull();
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
      const handoff = { id: 'h1', reason: 'solve the CAPTCHA', profile: 'job-hunt' };
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

    it('parks a document hand-off, ahead of the asks already waiting, the way the read orders it', () => {
      const kinds = (held: SessionRecord): unknown[] =>
        held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

      const asked = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: { tool_call: { tool_call_id: 'toolu_q' }, prompt: { question: 'which?' } },
        },
      });
      const ask = { id: 'd1', path: '/proj/docs/plan.md', reviewable: true };
      const parked = applyUpdate(asked, { document_hand_off_pending: { key: SLOT, ...ask } });

      expect(kinds(parked), 'the ask leads the question that was already waiting').toEqual([
        'document_hand_off',
        'question',
      ]);
      expect(
        kinds(applyUpdate(parked, { document_hand_off_resolved: { key: SLOT, id: 'd1' } })),
        'and its resolution falls back to the question',
      ).toEqual(['question']);
      expect(
        applyUpdate(parked, { document_hand_off_resolved: { key: SLOT, id: 'd2' } }),
        'a resolution naming another hand-off leaves this one parked',
      ).toBe(parked);
    });

    it('parks a held config write ahead of the asks already waiting, and clears it by its own id', () => {
      const kinds = (held: SessionRecord): unknown[] =>
        held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

      const asked = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: { tool_call: { tool_call_id: 'toolu_q' }, prompt: { question: 'which?' } },
        },
      });
      const ask = {
        id: 'w1',
        edits: [
          {
            key: 'orgs.Default.projects.forge.max_workers',
            current: 2,
            proposed: 4,
            unset: false,
          },
        ],
      };
      const parked = applyUpdate(asked, { settings_write_pending: { key: SLOT, ask } });

      expect(kinds(parked), 'the write leads the question that was already waiting').toEqual([
        'settings_write',
        'question',
      ]);
      expect(
        kinds(applyUpdate(parked, { settings_write_resolved: { key: SLOT, id: 'w1' } })),
        'and its resolution falls back to the question',
      ).toEqual(['question']);
      expect(
        applyUpdate(parked, { settings_write_resolved: { key: SLOT, id: 'w2' } }),
        'a resolution naming another write leaves this one parked',
      ).toBe(parked);
    });

    it('parks a held vault call ahead of the asks already waiting, and clears it by its own id', () => {
      const kinds = (held: SessionRecord): unknown[] =>
        held.pending_asks.map((ask) => (ask as Record<string, unknown>)['kind']);

      const asked = applyUpdate(empty(), {
        question_request: {
          key: SLOT,
          tool_id: 'toolu_q',
          request: { tool_call: { tool_call_id: 'toolu_q' }, prompt: { question: 'which?' } },
        },
      });
      const draft = {
        id: 'v1',
        tool: 'vault__get',
        summary: "read the password for 'bank'",
        needs_unlock: true,
      };
      const parked = applyUpdate(asked, { vault_ask_pending: { key: SLOT, draft } });

      expect(kinds(parked), 'the call leads the question that was already waiting').toEqual([
        'vault',
        'question',
      ]);
      expect(
        kinds(applyUpdate(parked, { vault_ask_resolved: { key: SLOT, id: 'v1' } })),
        'and its resolution falls back to the question',
      ).toEqual(['question']);
      expect(
        applyUpdate(parked, { vault_ask_resolved: { key: SLOT, id: 'v2' } }),
        'a resolution naming another call leaves this one parked',
      ).toBe(parked);
    });

    it('merges a restamped vault call in place when another call unlocks', () => {
      const draft = {
        id: 'v1',
        tool: 'vault__get',
        summary: "read the password for 'bank'",
        needs_unlock: true,
      };
      const first = applyUpdate(empty(), { vault_ask_pending: { key: SLOT, draft } });
      const both = applyUpdate(first, {
        vault_ask_pending: { key: SLOT, draft: { ...draft, id: 'v2' } },
      });
      const rebased = applyUpdate(both, {
        vault_ask_pending: { key: SLOT, draft: { ...draft, needs_unlock: false } },
      });

      const held = (asked: SessionRecord): { id: string; needsUnlock: boolean }[] =>
        asked.pending_asks.map((ask) => {
          const request = (ask as Record<string, unknown>)['request'] as Record<string, unknown>;
          return { id: request['id'] as string, needsUnlock: request['needs_unlock'] as boolean };
        });

      expect(held(rebased), 'the restamp updates the call and keeps its place').toEqual([
        { id: 'v1', needsUnlock: false },
        { id: 'v2', needsUnlock: true },
      ]);
    });
  });

  describe('the composer', () => {
    it('runs a take from its first reading to the notice it leaves', () => {
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      expect(started.composer.take).toMatchObject({ phase: 'recording', floor_db: -50 });

      const heard = applyUpdate(started, {
        dictate_level: { key: SLOT, peak_db: -20, generation: 1 },
      });
      // The meter draws fractions of the take's own range, computed where the
      // server computes them: the reading, its own floor, and the ceiling.
      expect(take(heard)['levels']).toEqual([0.6]);
      expect(take(heard)['peak_db']).toBe(-20);

      const transcribing = applyUpdate(heard, {
        dictate_transcribing: { key: SLOT, generation: 1 },
      });
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
          surface: 'review',
        },
      });
      expect(ended.composer.take).toBeNull();
      expect(ended.composer.notice).toEqual({
        kind: 'landed',
        text: 'hello',
        truncated: false,
        generation: 1,
        // **The surface that began the take crosses with its words**,
        // so a surface reading the live notice can refuse one that is not its
        // own - the record's re-hand says the same thing.
        surface: 'review',
      });
    });

    /**
     * **One perf record per received dictate frame, carrying the phase the
     * record's take showed before the frame.** That `before` is what answers
     * a take whose transcribing row vanished: whether the row was cleared
     * before the words arrived, and by which frame.
     */
    it('writes one perf record per dictate frame with the phase before it', () => {
      const records: PerfRecord[] = [];
      const probe = startPerfProbe({
        sink: (batch) => records.push(...batch),
        schedule: () => () => {},
      });
      try {
        const started = applyUpdate(empty(), {
          dictate_started: { key: SLOT, floor_db: -50, generation: 7 },
        });
        const transcribing = applyUpdate(started, {
          dictate_transcribing: { key: SLOT, generation: 7 },
        });
        applyUpdate(transcribing, {
          dictate_ended: {
            key: SLOT,
            outcome: { landed: { text: 'hello', truncated: false } },
            generation: 7,
          },
        });
      } finally {
        probe?.stop();
      }

      const frames = records.filter((record) => record.kind === 'dictate');
      expect(
        frames.map((record) => [record.phase, record.before, record.generation, record.session]),
      ).toEqual([
        ['started', 'none', 7, 'Busytools/forge/lead'],
        ['transcribing', 'recording', 7, 'Busytools/forge/lead'],
        ['ended', 'transcribing', 7, 'Busytools/forge/lead'],
      ]);
    });

    /**
     * **The frames the reducer drops are the incident's own shape, and they
     * are recorded too.** A take whose row was cleared before its words
     * arrived answers `dictate_ended` with no take held (`before: none`), and
     * a superseded take's frame still names its generation. The record is
     * written at the handler's top for exactly that reason: moved below any
     * guard, the one frame this exists to capture goes unwritten while the
     * happy-path test stays green.
     */
    it('records a dictate frame the reducer drops, naming the phase it found', () => {
      const records: PerfRecord[] = [];
      const probe = startPerfProbe({
        sink: (batch) => records.push(...batch),
        schedule: () => () => {},
      });
      try {
        // The words arriving with no take on the record.
        applyUpdate(empty(), {
          dictate_ended: {
            key: SLOT,
            outcome: { landed: { text: 'late', truncated: false } },
            generation: 7,
          },
        });
        // A start whose frame carried no floor reaches the early return.
        applyUpdate(empty(), { dictate_started: { key: SLOT, generation: 8 } });
        // A superseded take's transcribing frame applies nothing.
        const live = applyUpdate(empty(), {
          dictate_started: { key: SLOT, floor_db: -50, generation: 9 },
        });
        applyUpdate(live, { dictate_transcribing: { key: SLOT, generation: 8 } });
      } finally {
        probe?.stop();
      }

      expect(
        records
          .filter((record) => record.kind === 'dictate')
          .map((record) => [record.phase, record.before, record.generation]),
      ).toEqual([
        ['ended', 'none', 7],
        ['started', 'none', 8],
        ['started', 'none', 9],
        ['transcribing', 'recording', 8],
      ]);
    });

    it('drops a reading that belongs to no take', () => {
      const held = empty();
      expect(applyUpdate(held, { dictate_level: { key: SLOT, peak_db: -20 } })).toBe(held);
    });

    /**
     * **A take's readings and its transcribing are that take's.** The two
     * frames carry the take's own generation like their siblings, so one
     * that lands after a newer start cannot write its meter into the live
     * take or move it to `transcribing` while the microphone still records
     * (#839).
     */
    it("drops a superseded take's reading and transcribing", () => {
      const first = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const second = applyUpdate(first, {
        dictate_started: { key: SLOT, floor_db: -40, generation: 2 },
      });

      const stale = applyUpdate(second, {
        dictate_level: { key: SLOT, peak_db: -20, generation: 1 },
      });
      expect(stale, "the older take's reading is not the live meter").toBe(second);

      const moved = applyUpdate(second, {
        dictate_transcribing: { key: SLOT, generation: 1 },
      });
      expect(take(moved)['phase'], 'nor does it move the live take to transcribing').toBe(
        'recording',
      );
    });

    /**
     * **A server one release behind sends neither frame with a generation.**
     * Absence is this take's - the same tolerance `Take.generation` and
     * `dispatch_id` get - or a new client would drop every reading and the
     * transcribing phase of a take that is running (#839).
     */
    it("takes a frame with no generation as this take's", () => {
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const heard = applyUpdate(started, { dictate_level: { key: SLOT, peak_db: -20 } });
      expect(take(heard)['peak_db'], 'the older server feeds the live meter').toBe(-20);

      const moved = applyUpdate(heard, { dictate_transcribing: { key: SLOT } });
      expect(take(moved)['phase'], 'and its transcribing phase moves the take').toBe(
        'transcribing',
      );
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
        generation: 1,
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

    it('answers a refusal with the notice, and never with the drawn take', () => {
      // The refusal a client meets: it started a take locally and the server
      // turned the start down - a second take on a busy seat, or on another
      // seat of this connection - so nothing was ever drawn, and the line is
      // the core's wording, passed through.
      const ended = applyUpdate(empty(), {
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
        generation: 0,
      });

      // **And the state a refusal must never resolve**: a take IS drawn -
      // this connection's own, still live on the server - so a line about a
      // start that never ran must not clear it and empty the screen while
      // the recording runs (#1880). A future path that dispatches a start
      // while a take is drawn lands here.
      const started = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });
      const kept = applyUpdate(started, {
        dictate_ended: {
          key: SLOT,
          outcome: { refused: { message: 'another client is already dictating on this session' } },
          generation: 0,
        },
      });

      expect(kept, 'a refusal resolved the take this connection is recording').toBe(started);
    });

    it('keeps the take for a progress report that carries no counts', () => {
      const held = applyUpdate(empty(), {
        dictate_started: { key: SLOT, floor_db: -50, generation: 1 },
      });

      const next = applyUpdate(held, { dictate_progress: { key: SLOT, generation: 1 } });

      expect(next, 'a report with nothing in it narrowed the take').toBe(held);
    });

    it('compacts on the status frame and stops on the null that clears it', () => {
      const compacting = applyUpdate(empty(), {
        chat_appended: {
          key: SLOT,
          msg: { type: 'system', subtype: 'status', status: 'compacting' },
        },
      });
      expect(compacting.composer.compacting, 'the flag is what the frame sets').toBe(true);
      expect(machineInForce(compacting.composer), 'and it owns the box').toBe('compacting');

      const settled = applyUpdate(compacting, {
        chat_appended: { key: SLOT, msg: { type: 'system', subtype: 'status', status: null } },
      });
      expect(settled.composer.compacting).toBe(false);
      expect(machineInForce(settled.composer)).toBeNull();
    });

    it('leaves a handoff standing when a compaction settles beside it', () => {
      // The handoff comes off the record read, which no frame carries - so a
      // status null is news about the compaction and must not hand the
      // composer back while the seat is still being replaced.
      const held = empty();
      const replacing = { ...held, composer: { ...held.composer, machine: 'handoff' as const } };

      const settled = applyUpdate(replacing, {
        chat_appended: { key: SLOT, msg: { type: 'system', subtype: 'status', status: null } },
      });
      expect(settled.composer.machine, 'the handoff is not a compaction').toBe('handoff');
      expect(machineInForce(settled.composer), 'and it still owns the box').toBe('handoff');
    });

    it('keeps a handoff over a compaction whose status frame arrives beside it', () => {
      // A handoff outranks a compaction: the flag is kept apart from the
      // machine turn, so a compaction cannot displace the handoff - and it is
      // still there when the handoff ends.
      const held = empty();
      const replacing = { ...held, composer: { ...held.composer, machine: 'handoff' as const } };

      const live = applyUpdate(replacing, {
        chat_appended: {
          key: SLOT,
          msg: { type: 'system', subtype: 'status', status: 'compacting' },
        },
      });
      expect(live.composer.machine, 'a compaction does not displace the handoff').toBe('handoff');
      expect(machineInForce(live.composer), 'the handoff is in force').toBe('handoff');

      const ended = applyUpdate(live, { machine_turn: { key: SLOT, machine: null } });
      expect(ended.composer.machine, 'the handoff is over').toBeNull();
      expect(
        machineInForce(ended.composer),
        'and the compaction it ran over takes the box rather than leaving it live for a round trip',
      ).toBe('compacting');
    });

    it('takes the machine turn the core announces, both ways', () => {
      // A page already open never re-reads the record for a handoff, so these
      // two frames are the whole of what stands the composer down and hands
      // it back.
      const began = applyUpdate(empty(), {
        machine_turn: { key: SLOT, machine: 'handoff' },
      });
      expect(began.composer.machine).toBe('handoff');

      const ended = applyUpdate(began, { machine_turn: { key: SLOT, machine: null } });
      expect(ended.composer.machine).toBeNull();
    });
  });

  /**
   * The override set a `/dictate` edit leaves behind. The core echoes the whole
   * set on an update of its own after every set and reset - so it is folded,
   * the way the view this client replaced folded it, rather than left to a
   * read.
   */
  describe('the dictate override echo', () => {
    it("leaves the record alone: the axes are this client's own now", () => {
      // The core still echoes `dictate_overrides` for the `/dictate` overlay
      // of the view this client replaced, and this client holds its axes
      // itself - so the echo is an update with nothing here to write it to,
      // and the record stands rather than gaining a field nothing reads.
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
 * Every variant `SessionUpdate` carries - 82 of them - read off the enum in
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
  'call_output',
  'session_replaced',
  'handoff',
  'machine_turn',
  'connection_failed',
  'releasing',
  'notice',
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
  'forge_account_identity',
  'dictate_overrides',
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
  'subagent_cards_changed',
  'file_index_changed',
  'processes_changed',
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
  'document_hand_off_pending',
  'document_hand_off_resolved',
  'settings_write_pending',
  'settings_write_resolved',
  'vault_ask_pending',
  'vault_ask_resolved',
  'voice_audio',
  'voice_mode_state',
  'terminal_hand_off_pending',
  'terminal_hand_off_resolved',
  'terminal_changed',
  'biometric_enrolments_changed',
  'vault_rotated',
  'vault_lock_changed',
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
      'the census no longer carries every variant the enum declares (82 of them): a truncated ' +
        'census leaves the assertions below checking only the names it still has',
    ).toBe(82);
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

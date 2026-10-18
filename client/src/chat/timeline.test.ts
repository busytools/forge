import { describe, expect, it } from 'vitest';

import { dispatchFrames, dispatchTimeline } from './timeline';

/** The dispatch itself, as the session's own assistant frame carries it. */
const dispatch = (over: Record<string, unknown> = {}) => ({
  type: 'assistant',
  parent_tool_use_id: null,
  message: {
    role: 'assistant',
    content: [
      {
        type: 'tool_use',
        id: 'tu-task',
        name: 'Task',
        input: {
          description: 'review the fold',
          subagent_type: 'code-reviewer',
          prompt: 'Read the pane and report two nits.',
          ...over,
        },
      },
    ],
  },
});

/** One call the instance made. */
const call = (id: string, name: string, input: unknown) => ({
  type: 'assistant',
  parent_tool_use_id: 'tu-task',
  message: { role: 'assistant', content: [{ type: 'tool_use', id, name, input }] },
});

/** The frame that answers a call. */
const answer = (id: string, text: string, isError = false) => ({
  type: 'user',
  parent_tool_use_id: 'tu-task',
  message: {
    role: 'user',
    content: [{ type: 'tool_result', tool_use_id: id, content: text, is_error: isError }],
  },
});

/** What the instance wrote between calls. */
const said = (text: string) => ({
  type: 'assistant',
  parent_tool_use_id: 'tu-task',
  message: { role: 'assistant', content: [{ type: 'text', text }] },
});

describe('a dispatch read into its timeline', () => {
  it('reads the instance own calls in order, each with what it ran and came back with', () => {
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Grep', { pattern: 'subagent' }),
        answer('c1', '3 matches'),
        said('The pane folds its own cards.'),
        call('c2', 'Bash', { command: 'git log --oneline -3' }),
        answer('c2', 'a1b2c3 fold the cards'),
      ],
      'tu-task',
    );

    expect(
      frames.lines.map((line) => (line.kind === 'call' ? line.leaf.title : line.text)),
      'calls and prose in the order they arrived',
    ).toEqual(['subagent', 'The pane folds its own cards.', 'git log --oneline -3']);
    const first = frames.lines[0];
    expect(
      first?.kind === 'call' ? JSON.stringify(first.leaf.body) : '',
      'the matches it came back with, on the leaf the session own row draws',
    ).toContain('3 matches');
    expect(first?.kind === 'call' && first.leaf.status).toBe('completed');
  });

  it('marks an errored answer as the failure it is', () => {
    const frames = dispatchFrames(
      [dispatch(), call('c1', 'Read', { file_path: 'gone.rs' }), answer('c1', 'ENOENT', true)],
      'tu-task',
    );

    const first = frames.lines[0];
    expect(first?.kind === 'call' && first.leaf.status).toBe('failed');
  });

  it('keeps the instance own thinking, which the session draws as a row', () => {
    // Measured: thinking blocks arrive inside dispatched frames, and dropping
    // them whole would be the one frame type with no row at all.
    const frames = dispatchFrames(
      [
        {
          type: 'assistant',
          parent_tool_use_id: 'tu-task',
          message: {
            role: 'assistant',
            content: [{ type: 'thinking', thinking: 'Two files, then the diff.' }],
          },
        },
      ],
      'tu-task',
    );

    const first = frames.lines[0];
    expect(first?.kind, 'a thought is a line of its own').toBe('thought');
    expect(first?.kind === 'thought' && first.text).toBe('Two files, then the diff.');
  });

  it('keeps a backgrounded child call open rather than settling it on its ack', () => {
    // The session's own rule: a call whose task outlives its turn draws as
    // running however clean the launch ack it was answered with.
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Bash', { command: 'sleep 300', run_in_background: true }),
        answer('c1', 'Command running in background with ID: bg1.'),
        {
          type: 'system',
          subtype: 'task_started',
          tool_use_id: 'c1',
          task_id: 'bg1',
          is_backgrounded: true,
        },
      ],
      'tu-task',
    );

    const first = frames.lines[0];
    expect(first?.kind === 'call' && first.leaf.status, 'still running').toBe('in_progress');
  });

  it('carries the brief, a named model and an isolation request off the dispatch', () => {
    const frames = dispatchFrames(
      [dispatch({ model: 'claude-opus-5', isolation: 'worktree' })],
      'tu-task',
    );

    expect(frames.brief).toBe('Read the pane and report two nits.');
    expect(frames.model).toBe('claude-opus-5');
    expect(frames.isolation).toBe('worktree');
  });

  it('carries where the CLI wrote the task own transcript', () => {
    // The notification's `summary` is deliberately NOT read: it repeats the
    // agent's final message, which the timeline already draws as its own
    // prose, so drawing it too showed the same report twice.
    const frames = dispatchFrames(
      [
        dispatch(),
        {
          type: 'system',
          subtype: 'task_notification',
          tool_use_id: 'tu-task',
          task_id: 't1',
          status: 'completed',
          output_file: '/tmp/tasks/t1.output',
        },
      ],
      'tu-task',
    );

    expect(frames.outputFile).toBe('/tmp/tasks/t1.output');
  });

  it('carries the roster own facts: the task handle, the depth and the kind', () => {
    const frames = dispatchFrames(
      [
        dispatch(),
        {
          type: 'system',
          subtype: 'task_started',
          tool_use_id: 'tu-task',
          task_id: 't1',
          spawn_depth: 1,
          task_type: 'local_agent',
        },
      ],
      'tu-task',
    );

    expect(frames.taskId).toBe('t1');
    expect(frames.depth).toBe(1);
    expect(frames.taskType).toBe('local_agent');
  });

  it('beats on the call the heartbeat names', () => {
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Bash', { command: 'just check' }),
        {
          type: 'tool_progress',
          tool_use_id: 'c1-heartbeat-1',
          tool_name: 'Bash',
          parent_tool_use_id: 'c1',
        },
      ],
      'tu-task',
    );

    expect(frames.beats.has('c1'), 'the open call carries the pulse').toBe(true);
  });

  it('forgets the beat once the call comes back', () => {
    // A beaten call that settled must not keep drawing its heartbeat.
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Bash', { command: 'just check' }),
        {
          type: 'tool_progress',
          tool_use_id: 'c1-heartbeat-1',
          tool_name: 'Bash',
          parent_tool_use_id: 'c1',
        },
        answer('c1', 'done'),
      ],
      'tu-task',
    );

    expect(frames.beats.has('c1'), 'the beat is gone with the answer').toBe(false);
  });

  it('settles a child from a notification that names its own call', () => {
    // The frame carries the call id itself, so a notification whose
    // `task_started` was never seen still ends the call it names.
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Bash', { command: 'sleep 300' }),
        answer('c1', 'Command running in background with ID: bg1.'),
        {
          type: 'system',
          subtype: 'task_notification',
          tool_use_id: 'c1',
          task_id: 'bg1',
          status: 'stopped',
        },
      ],
      'tu-task',
    );

    const first = frames.lines[0];
    expect(first?.kind === 'call' && first.leaf.status, 'a stopped task is a kill').toBe('killed');
  });

  it('hands an edit its own record, so its hunks draw as the session draws them', () => {
    const frames = dispatchFrames(
      [
        dispatch(),
        call('c1', 'Edit', { file_path: 'a.rs', old_string: 'one', new_string: 'two' }),
        {
          type: 'user',
          parent_tool_use_id: 'tu-task',
          message: {
            role: 'user',
            content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'ok' }],
          },
          tool_use_result: {
            filePath: 'a.rs',
            structuredPatch: [
              { oldStart: 1, oldLines: 1, newStart: 1, newLines: 1, lines: ['-one', '+two'] },
            ],
          },
        },
      ],
      'tu-task',
    );

    // The record's patch, not the two-sided fallback: without the record the
    // fold still draws a diff from old_string/new_string, so only the hunk
    // KIND discriminates - the CLI's own hunks carry their header and line
    // numbers, and the fallback has neither.
    const first = frames.lines[0];
    const body = first?.kind === 'call' ? first.leaf.body : [];
    expect(
      body.map((piece) => piece.kind),
      'the CLI own hunk, not the fallback two-sided diff',
    ).toEqual(['hunk']);
    const piece = body[0];
    expect(piece?.kind === 'hunk' ? piece.header : '', 'with its line numbers').toBe(
      '@@ -1,1 +1,1 @@',
    );
  });

  it('keeps another dispatch frames out of this one', () => {
    const other = {
      ...call('x1', 'Bash', { command: 'echo other' }),
      parent_tool_use_id: 'tu-other',
    };
    const frames = dispatchFrames(
      [dispatch(), other, call('c1', 'Read', { file_path: 'a.rs' })],
      'tu-task',
    );

    expect(frames.lines, 'only this dispatch own frames').toHaveLength(1);
  });

  it('is empty for a dispatch whose frames did not survive', () => {
    // The resumed-session bound: the read holds the dispatch and its answer,
    // and none of what ran under it.
    const frames = dispatchFrames([dispatch()], 'tu-task');

    expect(frames.lines).toEqual([]);
    expect(frames.brief, 'the brief still reads off the dispatch itself').toBe(
      'Read the pane and report two nits.',
    );
  });
});

describe('the page frames a timeline reads', () => {
  /** The whole-walk answer, which the carried one may never differ from. */
  const whole = (turns: { messages: unknown[] }[], dispatchId: string) =>
    dispatchFrames(
      turns.flatMap((turn) => turn.messages),
      dispatchId,
    );

  /**
   * **The carry may not move a line.** A dispatch's own frames can arrive in
   * turns after the one that opened it - a result answering an earlier turn's
   * call, a heartbeat whose answer lands later - so the carried read has to
   * equal the whole walk across the page, frame for frame.
   */
  it('answers the carried page as the whole walk does, across turns', () => {
    const turns = [
      {
        messages: [
          dispatch(),
          call('c1', 'Grep', { pattern: 'x' }),
          { type: 'system', subtype: 'task_started', tool_use_id: 'c1', task_id: 't1' },
          { type: 'tool_progress', parent_tool_use_id: 'c1' },
        ],
      },
      {
        messages: [
          answer('c1', '3 matches'),
          said('between'),
          call('c2', 'Bash', { command: 'true' }),
          // A backgrounded child call, its task opened in this turn. The wire
          // carries a task's status in `patch`, with no call id.
          call('c3', 'Bash', { command: 'sleep 300' }),
          { type: 'system', subtype: 'task_started', tool_use_id: 'c3', task_id: 't3' },
          { type: 'system', subtype: 'task_updated', task_id: 't3', patch: { status: 'running' } },
          answer('c3', 'Command running in background with ID: bg1.'),
        ],
      },
      {
        messages: [
          answer('c2', 'ok'),
          // A heartbeat whose answer never arrives stays on its line.
          { type: 'tool_progress', parent_tool_use_id: 'c2' },
          // A task settled by its patch alone, which names the task and never
          // the call it opened.
          { type: 'system', subtype: 'task_updated', task_id: 't1', patch: { status: 'failed' } },
          // And an ending, which does name the call.
          {
            type: 'system',
            subtype: 'task_notification',
            tool_use_id: 'c3',
            task_id: 't3',
            status: 'stopped',
          },
        ],
      },
    ];

    const carried = dispatchTimeline(turns, 'tu-task');
    expect(carried, 'the carried page').toEqual(whole(turns, 'tu-task'));
    // A result answers the call an EARLIER turn made, and the beat on the one
    // still running is the caller's: a per-turn read would drop both.
    expect(carried.beats, 'the open call keeps its beat').toEqual(new Set(['c2']));
    // The frames that settle a call are in a later turn than the call and
    // name it by task: an interest test that only knew the dispatch's own id
    // would leave both calls running for good.
    const status = (id: string): string | null => {
      const line = carried.lines.find((one) => one.kind === 'call' && one.leaf.id === id);
      return line?.kind === 'call' ? line.leaf.status : null;
    };
    expect(status('c1'), 'a task settled by its patch').toBe('failed');
    expect(status('c3'), 'a task stopped by its notification').toBe('killed');
    // The carried answer is the same one a second call gets.
    expect(dispatchTimeline(turns, 'tu-task'), 'answered again').toEqual(whole(turns, 'tu-task'));
  });

  /**
   * **Each dispatch carries its own frames.** Two instances on one page mean
   * two carries over the same turns: a shared kind would hand the second row
   * the first one's frames, and a filter that took every parented frame would
   * keep the other instance's.
   */
  it('keeps two dispatches apart, and answers each as its whole walk', () => {
    const other = (): unknown => ({
      type: 'assistant',
      parent_tool_use_id: null,
      message: {
        role: 'assistant',
        content: [{ type: 'tool_use', id: 'tu-other', name: 'Task', input: { prompt: 'sweep' } }],
      },
    });
    const underOther = (frame: Record<string, unknown>): unknown => ({
      ...frame,
      parent_tool_use_id: 'tu-other',
    });
    const turns = [
      {
        messages: [
          dispatch(),
          other(),
          call('c1', 'Grep', { pattern: 'x' }),
          underOther(call('o1', 'Bash', { command: 'true' })),
        ],
      },
      {
        messages: [answer('c1', '3 matches'), underOther(answer('o1', 'ok')), said('between')],
      },
    ];

    const first = dispatchTimeline(turns, 'tu-task');
    expect(first, 'the first dispatch').toEqual(whole(turns, 'tu-task'));
    const second = dispatchTimeline(turns, 'tu-other');
    expect(second, 'the second dispatch, off the same turns').toEqual(whole(turns, 'tu-other'));
    expect(dispatchTimeline(turns, 'tu-task'), 'and the first again').toEqual(
      whole(turns, 'tu-task'),
    );
    const ids = (lines: typeof first.lines): string[] =>
      lines.flatMap((line) => (line.kind === 'call' ? [line.leaf.id] : []));
    expect(ids(first.lines), "the first instance's own call").toEqual(['c1']);
    expect(ids(second.lines), "the second instance's, and nothing of the first").toEqual(['o1']);
  });

  /**
   * **A frame landing while the row is open is read on the next walk.** The
   * store grows a turn by replacing its array, which the carry follows; a
   * materialised copy of the page taken when the walk started would not.
   *
   * **The carried answer is compared, not a substring of it.** The second read
   * is the memo's own path, so a carry that failed to take the growth in would
   * still be answered from what it folded before - and a `toContain` on one
   * frame would miss every other line that went with it.
   */
  it('follows a turn that grows while the row is open', () => {
    const first = said('first');
    const second = said('second');
    const late = said('late');
    const turns = [{ messages: [first] }, { messages: [second] }];
    expect(dispatchTimeline(turns, 'tu-task').lines, 'before the growth').toEqual(
      whole(turns, 'tu-task').lines,
    );

    // The store's own growth shape: the turn replaced, its array extended.
    const grown = [{ messages: [first] }, { messages: [second, late] }];
    expect(dispatchTimeline(grown, 'tu-task'), 'the late frame is read').toEqual(
      whole(grown, 'tu-task'),
    );
    // The same page again: the second read is answered from the carry.
    expect(dispatchTimeline(grown, 'tu-task'), 'and answered from the carry').toEqual(
      whole(grown, 'tu-task'),
    );

    // **A push in place is a growth too.** Nothing in the store grows a turn
    // this way, but the carry's growth test is the only thing between a
    // caller that does and a frame nobody ever reads: the folded length is
    // what keeps it honest.
    const pushed = [{ messages: [first] }, { messages: [second] as unknown[] }];
    expect(dispatchTimeline(pushed, 'tu-task'), 'before the push').toEqual(
      whole(pushed, 'tu-task'),
    );
    (pushed[1]?.messages as unknown[]).push(said('pushed'));
    expect(dispatchTimeline(pushed, 'tu-task'), 'an in-place frame is read').toEqual(
      whole(pushed, 'tu-task'),
    );

    // **A cut in place is not a growth.** The store always replaces a turn's
    // array, so nothing shrinks one under the carry today - but a caller that
    // did would have the seam read it as an append of nothing, and the folded
    // length is what keeps the value off frames the page no longer has. The
    // cut's own frames, so another case's memo on the same opening cannot
    // answer for it.
    const cutBefore = said('cut-before');
    const cutAfter = said('cut-after');
    const cut = [{ messages: [first] }, { messages: [cutBefore, cutAfter] as unknown[] }];
    expect(dispatchTimeline(cut, 'tu-task'), 'before the cut').toEqual(whole(cut, 'tu-task'));
    (cut[1]?.messages as unknown[]).pop();
    expect(dispatchTimeline(cut, 'tu-task'), 'a cut in place is read afresh').toEqual(
      whole(cut, 'tu-task'),
    );

    // A REPLACEMENT cut, whose carried head makes the growth test run: a
    // shorter array in a new identity is a miss through `extendsFrom`'s own
    // length check, even without the folded length. The in-place case above is
    // what pins the guard; this pins the replacement shape beside it.
    const replHead = said('replacement-head');
    const replBefore = [
      { messages: [first] },
      { messages: [replHead, said('replacement-tail')] as unknown[] },
    ];
    expect(dispatchTimeline(replBefore, 'tu-task'), 'before the replacement').toEqual(
      whole(replBefore, 'tu-task'),
    );
    const replCut = [{ messages: [first] }, { messages: [replHead] as unknown[] }];
    expect(dispatchTimeline(replCut, 'tu-task'), 'a replacement cut is read afresh').toEqual(
      whole(replCut, 'tu-task'),
    );
  });
});

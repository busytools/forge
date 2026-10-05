import { describe, expect, it } from 'vitest';

import { dispatchFrames } from './timeline';

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

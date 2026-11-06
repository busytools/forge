import { describe, expect, it } from 'vitest';

import { joinFrom, outcomesFrom, outcomesOf } from './outcomes';

const sys = (fields: Record<string, unknown>) => ({ type: 'system', ...fields });

describe('the outcome join', () => {
  it('resolves an updated task through its start frame and keeps the end time', () => {
    // task_updated names only the task_id; the call is the link task_started
    // recorded, which is why the join keeps the owners map at all.
    const { calls, owners } = outcomesFrom([
      { messages: [sys({ subtype: 'task_started', task_id: 't1', tool_use_id: 'tu1' })] },
      {
        messages: [
          sys({
            subtype: 'task_updated',
            task_id: 't1',
            patch: { status: 'completed', end_time: 1000 },
          }),
        ],
      },
    ]);

    expect(calls.get('tu1')).toEqual({ failed: false, ended_ms: 1000, stopped: false });
    expect(owners.get('t1'), 'and the link is kept for a row that lacks its own').toBe('tu1');
  });

  it('marks a failed notification by its own tool_use_id, and latches it', () => {
    const map = outcomesFrom([
      {
        messages: [
          sys({ subtype: 'task_started', task_id: 't2', tool_use_id: 'tu2' }),
          sys({
            subtype: 'task_notification',
            task_id: 't2',
            tool_use_id: 'tu2',
            status: 'failed',
          }),
          sys({
            subtype: 'task_updated',
            task_id: 't2',
            patch: { status: 'completed', end_time: 2000 },
          }),
        ],
      },
    ]);

    expect(map.calls.get('tu2')?.failed, 'a later frame cannot un-fail the call').toBe(true);
    expect(map.calls.get('tu2')?.ended_ms, 'the updated frame still lands its end time').toBe(2000);
  });

  it('leaves a status the CLI did not call failed unmarked', () => {
    const map = outcomesFrom([
      {
        messages: [
          sys({
            subtype: 'task_notification',
            task_id: 't3',
            tool_use_id: 'tu3',
            status: 'stopped',
          }),
        ],
      },
    ]);

    expect(map.calls.get('tu3')?.failed, 'only failed paints the cross').toBe(false);
  });

  it('names a stopped task even when its launch is not on the page', () => {
    // The record holds the newest turns only, so a task launched earlier in
    // the session has no owner to resolve. The stop's own task id is enough to
    // stop the strip counting it, and no call outcome is invented without one.
    const map = outcomesFrom([
      {
        messages: [
          {
            type: 'assistant',
            message: {
              role: 'assistant',
              content: [
                {
                  type: 'tool_use',
                  id: 'call_stop',
                  name: 'TaskStop',
                  input: { task_id: 't-long' },
                },
              ],
            },
          },
          {
            type: 'user',
            message: {
              role: 'user',
              content: [
                {
                  type: 'tool_result',
                  tool_use_id: 'call_stop',
                  content: JSON.stringify({ message: 'Successfully stopped task: t-long' }),
                },
              ],
            },
          },
        ],
      },
    ]);

    expect(map.stoppedTasks.has('t-long'), 'the stop names the task it ended').toBe(true);
    expect(map.calls.size, 'and no call outcome is invented without a link').toBe(0);
  });

  /**
   * The record retains the join this was built from, so an outcomes map that
   * aliased it would let every reader of the strip write into the record.
   */
  it('hands out owner links that cannot write back into the join', () => {
    const join = joinFrom([
      { messages: [sys({ subtype: 'task_started', task_id: 't1', tool_use_id: 'tu1' })] },
    ]);
    const { owners } = outcomesOf(join);

    owners.set('t2', 'tu2');
    expect(owners.get('t1'), 'the outcome map lost its own link').toBe('tu1');
    expect(join.owners.has('t2'), 'the outcome map wrote through to the join').toBe(false);
  });

  /**
   * **Only a tool that starts work owns a task.** Any result may quote the
   * launch sentence - a `Read` of a log, a command echoing one - and the gate
   * is what keeps a quote from standing in for a launch whose real call is
   * paged out. Both halves matter: too wide and a quote claims a task, too
   * narrow and a real launch loses its call.
   */
  it('takes a launch sentence only from a tool that starts work', () => {
    const quoting = [
      {
        type: 'assistant',
        message: { content: [{ type: 'tool_use', id: 'call-read', name: 'Read' }] },
      },
      {
        type: 'user',
        message: {
          content: [
            {
              type: 'tool_result',
              tool_use_id: 'call-read',
              content: 'Command running in background with ID: t-quoted',
            },
          ],
        },
      },
    ];
    const launched = [
      {
        type: 'assistant',
        message: { content: [{ type: 'tool_use', id: 'call-bash', name: 'Bash' }] },
      },
      {
        type: 'user',
        message: {
          content: [
            {
              type: 'tool_result',
              tool_use_id: 'call-bash',
              content: 'Command running in background with ID: t-launched',
            },
          ],
        },
      },
    ];

    const quoted = outcomesFrom([{ messages: quoting }]);
    expect(quoted.owners.has('t-quoted'), 'a Read quoting the sentence claimed a task launch').toBe(
      false,
    );

    const real = outcomesFrom([{ messages: launched }]);
    expect(real.owners.get('t-launched'), 'a Bash launch did not link its task').toBe('call-bash');
  });
});

import { describe, expect, it } from 'vitest';

import { outcomesFrom } from './outcomes';

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

    expect(calls.get('tu1')).toEqual({ failed: false, ended_ms: 1000 });
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
});

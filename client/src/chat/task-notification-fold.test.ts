// A foreground call that outlives the CLI's task threshold opens a task, and
// its `task_notification` carries the call's own description as the summary.
// The call row already draws that description and is already settled by the
// task fact, so the notice is the same words twice while nothing else has been
// drawn since the call (#3088).
import { describe, expect, it } from 'vitest';

import { fold, type Unit, type WorkRow } from './units';

/** One assistant frame carrying a single Bash call. */
const call = (id: string, description: string, command = 'sleep 4; echo slow'): unknown => ({
  type: 'assistant',
  uuid: `a-${id}`,
  message: {
    id: `m-${id}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'tool_use', id, name: 'Bash', input: { command, description } }],
  },
});

/** One assistant frame carrying prose, which draws a row of its own. */
const said = (text: string): unknown => ({
  type: 'assistant',
  uuid: `t-${text}`,
  message: {
    id: 'm-text',
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
  },
});

/** The CLI's `task_started` for a foreground call. */
const started = (id: string, description: string): unknown => ({
  type: 'system',
  subtype: 'task_started',
  task_id: `task-${id}`,
  tool_use_id: id,
  description,
  is_backgrounded: false,
});

/** The CLI's `task_notification` for the same call. */
const notified = (
  id: string,
  summary: string,
  status = 'completed',
  extra: Record<string, unknown> = {},
): unknown => ({
  type: 'system',
  subtype: 'task_notification',
  task_id: `task-${id}`,
  tool_use_id: id,
  status,
  output_file: '',
  summary,
  ...extra,
});

/** The call's own result, which the fold draws on the call's row. */
const answered = (id: string): unknown => ({
  type: 'user',
  uuid: `r-${id}`,
  message: {
    role: 'user',
    content: [{ type: 'tool_result', tool_use_id: id, content: 'done', is_error: false }],
  },
});

const rowsOf = (units: Unit[]): WorkRow[] =>
  units.flatMap((unit) => (unit.kind === 'leaves' ? unit.rows : []));

const callsOf = (units: Unit[]): Extract<WorkRow, { tag: 'call' }>[] =>
  rowsOf(units).filter((row): row is Extract<WorkRow, { tag: 'call' }> => row.tag === 'call');

const noticesOf = (units: Unit[]): string[] =>
  units.flatMap((unit) => (unit.kind === 'notice' ? [unit.notice.text] : []));

describe('a foreground call ending with its task notification', () => {
  it('draws one row whose own status says it finished', () => {
    const units = fold([
      call('t1', 'Slow probe'),
      started('t1', 'Slow probe'),
      notified('t1', 'Slow probe'),
    ]);
    const calls = callsOf(units);
    expect(calls.map((row) => row.leaf.title)).toEqual(['Slow probe']);
    expect(calls[0]?.leaf.status).toBe('completed');
    expect(noticesOf(units)).toEqual([]);
  });

  it('keeps the notice when other rows landed after the call', () => {
    const units = fold([
      call('t1', 'Slow probe'),
      said('Working on the rest'),
      notified('t1', 'Slow probe'),
    ]);
    expect(noticesOf(units)).toEqual(['Slow probe']);
    expect(callsOf(units)).toHaveLength(1);
  });

  it('always draws a failed notification', () => {
    const units = fold([
      call('t1', 'Slow probe'),
      started('t1', 'Slow probe'),
      notified('t1', 'Slow probe', 'failed'),
    ]);
    expect(noticesOf(units)).toEqual(['Slow probe']);
  });

  it('keeps a summary the call row does not already say', () => {
    const units = fold([
      call('t1', 'Watch CI'),
      notified('t1', 'Background command "Watch CI" completed (exit code 0)'),
    ]);
    expect(noticesOf(units)).toEqual(['Background command "Watch CI" completed (exit code 0)']);
  });

  it('matches the call by id, not by the shared description', () => {
    // The ending names the FIRST call while the second is the run's newest
    // row: the first call's row is not the tail, so the notice draws.
    const held = fold([call('t1', 'probe'), call('t2', 'probe'), notified('t1', 'probe')]);
    expect(noticesOf(held)).toEqual(['probe']);

    // The ending names the newest call: its own row carries the words.
    const folded = fold([call('t1', 'probe'), call('t2', 'probe'), notified('t2', 'probe')]);
    expect(noticesOf(folded)).toEqual([]);
    expect(callsOf(folded).map((row) => row.leaf.id)).toEqual(['t1', 't2']);
  });

  it('keeps a quick call at one row, with no task frames at all', () => {
    const units = fold([call('t1', 'Quick probe', 'echo quick'), answered('t1')]);
    const calls = callsOf(units);
    expect(calls.map((row) => row.leaf.title)).toEqual(['Quick probe']);
    expect(calls[0]?.leaf.status).toBe('completed');
    expect(noticesOf(units)).toEqual([]);
  });

  it('draws one row when the page carries the same call without task frames', () => {
    const live = fold([
      call('t1', 'Slow probe'),
      started('t1', 'Slow probe'),
      notified('t1', 'Slow probe'),
    ]);
    const page = fold([call('t1', 'Slow probe'), answered('t1')]);
    expect(noticesOf(live)).toEqual([]);
    expect(noticesOf(page)).toEqual([]);
    expect(callsOf(live)).toHaveLength(1);
    expect(callsOf(page)).toHaveLength(1);
  });
});

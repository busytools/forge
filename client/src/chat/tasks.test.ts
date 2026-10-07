import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { tasks } from './tasks.svelte';
import type { TaskStripRow } from '../session/view';

/**
 * The change signal itself, which needs no DOM: what lights a row, what does
 * not, and when the light goes out.
 */
const row = (over: Partial<TaskStripRow> = {}): TaskStripRow => ({
  id: 't1',
  status: 'in_progress',
  subject: 'a task',
  owner: null,
  meta: 'in progress',
  ...over,
});

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  tasks.sync(null);
  vi.useRealTimers();
});

describe('the tasks store', () => {
  it('lights nothing on the first sync: a page opening is not a change', () => {
    tasks.sync([row()]);

    expect(tasks.lit('t1'), 'the first read lit a row').toBe(false);
  });

  it('lights the row of a task whose status moved, and clears it after the beat', () => {
    tasks.sync([row()]);
    tasks.sync([row({ status: 'completed' })]);

    expect(tasks.lit('t1'), 'the moved task lit nothing').toBe(true);

    vi.advanceTimersByTime(6500);
    expect(tasks.lit('t1'), 'the light stayed on past its beat').toBe(false);
  });

  it('lights a task that arrived, which is the set changing too', () => {
    tasks.sync([row()]);
    tasks.sync([row(), row({ id: 't2', subject: 'arrived' })]);

    expect(tasks.lit('t2'), 'an arriving task lit nothing').toBe(true);
    expect(tasks.lit('t1'), 'and the untouched one lit').toBe(false);
  });

  it('leaves an untouched list dark, and a removal is not an error', () => {
    tasks.sync([row(), row({ id: 't2' })]);
    tasks.sync([row(), row({ id: 't2' })]);

    expect(tasks.lit('t1') || tasks.lit('t2'), 'an unchanged list lit').toBe(false);

    tasks.sync([row()]);
    expect(tasks.count().total, 'the removed task left the count').toBe(1);
  });

  it('counts what is done of what there is', () => {
    tasks.sync([row(), row({ id: 't2', status: 'completed' })]);

    expect(tasks.count()).toEqual({ done: 1, total: 2 });
  });
});

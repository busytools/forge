import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import TasksSegment from './TasksSegment.svelte';
import { tasks } from './tasks.svelte';

afterEach(() => {
  tasks.sync(null);
});

const TASKS = [
  {
    id: 't1',
    status: 'in_progress' as const,
    display: 'Landing the schedules row',
    subject: 'Land the schedules row',
    owner: 'lead',
    meta: 'in progress \u{b7} 2h',
  },
  {
    id: 't2',
    status: 'completed' as const,
    display: 'Port the cmdline rule',
    subject: 'Port the cmdline rule',
    owner: null,
    meta: 'completed',
  },
];

describe('the tasks segment', () => {
  it('carries the tasks glyph and how much of the set is done', () => {
    tasks.sync(TASKS);
    const body = render(TasksSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-tasks');
    expect(body, 'the count reads done of total').toContain('1 of 2');
  });

  it('draws nothing for a project that holds no tasks', () => {
    tasks.sync([]);
    const body = render(TasksSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});

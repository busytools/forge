import { render } from 'svelte/server';
import { afterEach, describe, expect, it } from 'vitest';

import type { BackgroundTask, ProcessSnapshot } from '../session/wire';
import ProcessesSegment from './ProcessesSegment.svelte';
import { processes } from './processes.svelte';

const walk = (tick: number, memory = 512): ProcessSnapshot => ({
  processes: [
    {
      pid: 8842,
      parent_pid: 1,
      name: 'cargo',
      command: 'cargo nextest run -p forge-web',
      memory_bytes: memory,
    },
  ],
  scanned_at: { secs_since_epoch: tick, nanos_since_epoch: 0 },
});

const registryRow = (over: Partial<BackgroundTask> = {}): BackgroundTask => ({
  task_id: 't-1',
  task_type: 'local_bash',
  description: 'Run the full suite',
  command: 'cargo nextest run',
  tool_use_id: 'tu-1',
  ...over,
});

// The reset lives in afterEach, not at each test's end: an assertion that
// fails must not skip the singleton's cleanup and cascade into the next test.
afterEach(() => {
  processes.sync(null, [], false);
});

describe('the processes segment', () => {
  it('carries the glyph, the running count, the combined memory and the mark', () => {
    processes.sync(
      walk(1),
      [registryRow(), registryRow({ task_id: 't-2', command: 'tail -f other.log' })],
      true,
    );
    const body = render(ProcessesSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-processes');
    // The joined string, separators spaced: Svelte trims whitespace at a
    // block's start, so the same line written with blocks loses the space
    // before each separator.
    expect(body, 'the count and the memory read as one spaced line').toContain(
      '2 running \u{b7} 512 B',
    );
    expect(body, 'a ring while work is in flight').toContain('class="ring"');
  });

  it('wears the check once everything has settled, never a spinner over zero', () => {
    processes.sync(walk(1), [registryRow()], true);
    processes.sync(walk(2), [], true);
    const body = render(ProcessesSegment, {}).body;

    expect(body, 'the zero and the settled count read as one spaced line').toContain(
      '0 running \u{b7} 1 settled',
    );
    expect(body, 'the check replaces the spinner').toContain('i-check');
    expect(body, 'no spinner over nothing running').not.toContain('class="ring"');
  });

  it('draws nothing for a seat with no batch work', () => {
    processes.sync(walk(1), [], true);
    const body = render(ProcessesSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });

  it('draws nothing for a seat whose only registry rows are not batch work', () => {
    processes.sync(walk(1), [registryRow({ task_type: 'local_agent' })], true);
    const body = render(ProcessesSegment, {}).body;

    expect(body, 'an agent task draws nothing here').not.toContain('sg-seg');
  });
});

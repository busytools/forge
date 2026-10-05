import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import type { SubagentCard } from '../session/wire';
import SubagentSegment from './SubagentSegment.svelte';
import { subagents } from './subagents.svelte';

/** One instance, as the record holds it. */
const card = (over: Partial<SubagentCard> = {}): SubagentCard => ({
  name: 'review the fold',
  dispatch_id: 'toolu_task',
  agent_type: 'code-reviewer',
  running: true,
  failed: false,
  backgrounded: false,
  ended_at: null,
  calls: 2,
  tail: [{ name: 'Grep', title: 'subagent', status: 'completed' }],
  usage: { total_tokens: 12_000, tool_uses: 2, duration_ms: 184_000 },
  ...over,
});

describe('the strip segment', () => {
  it('carries the subagents glyph, both counts, and the running mark', () => {
    subagents.sync([card(), card({ dispatch_id: 'toolu_other' })]);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'the glyph says what the segment is').toContain('i-subagents');
    expect(body, 'running and finished, session totals').toContain(
      '2 running \u{b7} 0 finished',
    );
    expect(body, 'a ring while any is working').toContain('class="ring"');
    subagents.sync(null);
  });

  it('counts the finished once none is running', () => {
    subagents.sync([
      card({ running: false }),
      card({ dispatch_id: 'toolu_other', running: false, failed: true }),
    ]);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'the finished count holds while the running one reads zero').toContain(
      '0 running \u{b7} 2 finished',
    );
    subagents.sync(null);
  });

  it('draws nothing for a seat with no instances', () => {
    subagents.sync(null);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});

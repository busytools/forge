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
  it('says how many are running while any is', () => {
    subagents.sync([card(), card({ dispatch_id: 'toolu_other' })]);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'the running count leads').toContain('2 agents running');
    subagents.sync(null);
  });

  it('says settled once none is', () => {
    subagents.sync([
      card({ running: false }),
      card({ dispatch_id: 'toolu_other', running: false, failed: true }),
    ]);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'the count is what matters most').toContain('2 agents settled');
    subagents.sync(null);
  });

  it('counts one agent in the singular', () => {
    subagents.sync([card()]);
    const body = render(SubagentSegment, {}).body;

    expect(body).toContain('1 agent running');
    subagents.sync(null);
  });

  it('draws nothing for a seat with no instances', () => {
    subagents.sync(null);
    const body = render(SubagentSegment, {}).body;

    expect(body, 'no segment at all').not.toContain('sg-seg');
  });
});

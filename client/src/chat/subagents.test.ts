import { describe, expect, it } from 'vitest';

import type { SubagentCard } from '../session/wire';
import { SubagentCards } from './subagents.svelte';

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
  tail: [],
  usage: null,
  ...over,
});

describe('the dispatch join', () => {
  it('finds the card by the id of the dispatch that opened it', () => {
    const cards = new SubagentCards();
    cards.sync([card(), card({ dispatch_id: 'toolu_other', name: 'other' })]);

    expect(cards.by('toolu_task')?.name, 'the card the row joins to').toBe('review the fold');
    expect(cards.by('toolu_none'), 'a call that opened no instance').toBeUndefined();
  });

  it('replaces the whole list on the next read', () => {
    const cards = new SubagentCards();
    cards.sync([card()]);
    cards.sync([card({ running: false, failed: true })]);

    const held = cards.by('toolu_task');
    expect(held?.running, 'the later read is the one held').toBe(false);
    expect(held?.failed).toBe(true);
  });

  it('clears for a record that holds none', () => {
    const cards = new SubagentCards();
    cards.sync([card()]);
    cards.sync([]);
    cards.sync(null);

    expect(cards.by('toolu_task'), 'no list is no cards').toBeUndefined();
  });
});

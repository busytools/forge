import { describe, expect, it } from 'vitest';

import type { SubagentCard } from '../session/wire';
import { SubagentCards, reveal, transcribable } from './subagents.svelte';

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

  it('keeps the list in dispatch order and counts the running', () => {
    const cards = new SubagentCards();
    cards.sync([card(), card({ dispatch_id: 'toolu_other', running: false })]);

    expect(
      cards.all().map((one) => one.dispatch_id),
      'the strip lists them in order',
    ).toEqual(['toolu_task', 'toolu_other']);
    expect(cards.running(), 'one of the two is still working').toBe(1);
  });
});

describe('which instances a list may lead to', () => {
  it('keeps the running and the ones with calls on the page', () => {
    const keeping = transcribable([
      card({ running: true, calls: 0 }),
      card({ dispatch_id: 'with-calls', running: false, calls: 3 }),
    ]);

    expect(keeping.map((one) => one.dispatch_id)).toEqual(['toolu_task', 'with-calls']);
  });

  it('drops an instance that ran before a restart or resume', () => {
    // The resumed bound: no frames are held for it, so its row would open
    // onto a brief and nothing - it stays out of any list that leads
    // somewhere.
    const keeping = transcribable([card({ running: false, calls: 0 })]);

    expect(keeping).toEqual([]);
  });
});

describe('revealing the row a dispatch drew', () => {
  it('opens the row, brings it into view and flashes it', () => {
    const held = {
      open: false,
      offsetWidth: 0,
      scrollIntoView: () => {},
      classList: { remove: () => {}, add: () => {} },
    };
    const root = {
      querySelector: (selector: string) => (selector.includes('toolu_task') ? held : null),
    } as unknown as ParentNode;

    expect(reveal('toolu_task', root), 'the row is on the page').toBe(true);
    expect(held.open, 'and the reveal opened it').toBe(true);
  });

  it('answers false for a row the page does not hold', () => {
    const root = { querySelector: () => null } as unknown as ParentNode;

    expect(reveal('toolu_gone', root), 'nothing to reveal').toBe(false);
  });
});

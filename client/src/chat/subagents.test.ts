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
    expect(cards.all().filter((one) => one.running).length, 'one of the two is still working').toBe(
      1,
    );
  });
});

describe('which instances a list may lead to', () => {
  const noneReachable = () => false;

  it('keeps the running even when nothing is reachable yet', () => {
    // A running instance's row is in the live turn; the set may lag one frame.
    const keeping = transcribable([card({ running: true })], noneReachable);

    expect(keeping.map((one) => one.dispatch_id)).toEqual(['toolu_task']);
  });

  it('keeps a settled one only when the page holds its row', () => {
    const cards = [
      card({ dispatch_id: 'held', running: false }),
      card({ dispatch_id: 'gone', running: false }),
    ];
    const keeping = transcribable(cards, (id) => id === 'held');

    expect(
      keeping.map((one) => one.dispatch_id),
      'the page is the judge, not the record',
    ).toEqual(['held']);
  });

  it('drops an instance that ran before a restart or resume', () => {
    // The resumed bound: no frames are held for it, so its row would open
    // onto a brief and nothing - it stays out of any list that leads
    // somewhere.
    const keeping = transcribable([card({ running: false, calls: 0 })], noneReachable);

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

  it('answers rather than throwing for an id that is not selector-safe', () => {
    // A quote would close the attribute selector early and throw SyntaxError;
    // the escaping is what keeps "answers false rather than throwing" true.
    const seen: string[] = [];
    const root = {
      querySelector: (selector: string) => {
        seen.push(selector);
        return null;
      },
    } as unknown as ParentNode;

    expect(reveal('we"ird', root)).toBe(false);
    expect(seen[0], 'the quote is escaped, not passed through').toContain('we\\"ird');
  });
});

import { describe, expect, it, vi } from 'vitest';

import type { SubagentCard } from '../session/wire';
import { SubagentCards, reachableIds, reveal, transcribable } from './subagents.svelte';

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

describe('the dispatches the loaded turns hold', () => {
  const dispatch = (id: string) => ({
    type: 'assistant',
    message: { content: [{ type: 'tool_use', id, name: 'Task', input: {} }] },
  });

  it('keeps every top-level tool_use id, across every loaded turn', () => {
    const ids = reachableIds([
      { messages: [dispatch('tu-one')] },
      {
        messages: [
          { type: 'user', message: { content: [{ type: 'text', text: 'and then?' }] } },
          dispatch('tu-two'),
        ],
      },
    ]);

    expect(ids.has('tu-one'), 'a dispatch in the first turn').toBe(true);
    expect(ids.has('tu-two'), 'and one in a later turn').toBe(true);
  });

  it('skips an instance own frames, which live inside a row rather than as one', () => {
    const ids = reachableIds([
      {
        messages: [
          dispatch('tu-one'),
          {
            type: 'assistant',
            parent_tool_use_id: 'tu-one',
            message: {
              content: [{ type: 'tool_use', id: 'tu-inside', name: 'Read', input: {} }],
            },
          },
        ],
      },
    ]);

    expect(ids.has('tu-one'), 'the dispatch itself').toBe(true);
    expect(ids.has('tu-inside'), 'the call under it is no dispatch').toBe(false);
  });
});

describe('revealing the row a dispatch drew', () => {
  it('opens the row, brings it into view and flashes it', () => {
    let seen: ScrollIntoViewOptions | undefined;
    const held = {
      open: false,
      offsetWidth: 0,
      scrollIntoView: (options?: ScrollIntoViewOptions) => {
        seen = options;
      },
      classList: { remove: () => {}, add: () => {} },
    };
    const root = {
      querySelector: (selector: string) => (selector.includes('toolu_task') ? held : null),
    } as unknown as ParentNode;

    expect(reveal('toolu_task', root), 'the row is on the page').toBe(true);
    expect(held.open, 'and the reveal opened it').toBe(true);
    // The one-motion landing: instant, and nearest, so a row the turn-scroll
    // already put on screen is not moved a second time.
    expect(seen, 'the jump is instant and nearest, never smooth or centred').toEqual({
      behavior: 'auto',
      block: 'nearest',
    });
  });

  it('falls back to the plain call row when no card names the id', () => {
    // A backgrounded bash has no subagent card; its row carries the fold's
    // own name, `call-c-<tool_use_id>`, which is what the fallback finds.
    const held = {
      open: false,
      offsetWidth: 0,
      scrollIntoView: () => {},
      classList: { remove: () => {}, add: () => {} },
    };
    const root = {
      querySelector: (selector: string) => (selector.includes('call-c-tu_bash') ? held : null),
    } as unknown as ParentNode;

    expect(reveal('tu_bash', root), 'a bash call reveals by its own row').toBe(true);
    expect(held.open, 'and the reveal opened it').toBe(true);
  });

  it('restarts the six-second flash when a second reveal lands inside it', () => {
    // The removal timer is kept per row: without that, the FIRST flash's
    // timer cuts the second one short and a quick re-click reads as no flash
    // at all.
    vi.useFakeTimers();
    try {
      const state = { hit: false };
      const row = {
        open: false,
        offsetWidth: 0,
        scrollIntoView: () => {},
        classList: {
          remove: (name: string) => {
            if (name === 'sg-hit') state.hit = false;
          },
          add: (name: string) => {
            if (name === 'sg-hit') state.hit = true;
          },
        },
      };
      const root = { querySelector: () => row } as unknown as ParentNode;

      reveal('tu_a', root);
      vi.advanceTimersByTime(700);
      expect(state.hit, 'the flash lit').toBe(true);

      vi.advanceTimersByTime(1300);
      reveal('tu_a', root);
      vi.advanceTimersByTime(700);

      vi.advanceTimersByTime(5400);
      expect(state.hit, 'the first timer must not cut the second flash short').toBe(true);

      vi.advanceTimersByTime(700);
      expect(state.hit, 'and the restarted window still ends').toBe(false);
    } finally {
      vi.useRealTimers();
    }
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

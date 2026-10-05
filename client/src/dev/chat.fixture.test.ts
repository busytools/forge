import { describe, expect, it } from 'vitest';

import fixture from './chat.fixture.json';

/** Every top-level `tool_use` id the canned turns carry, as the page reads them. */
function dispatchIds(): Set<string> {
  const ids = new Set<string>();
  for (const turn of fixture.turns) {
    for (const message of turn.messages) {
      const frame = message as {
        parent_tool_use_id?: unknown;
        message?: { content?: unknown };
      };
      const parent = frame.parent_tool_use_id;
      if (typeof parent === 'string' && parent !== '') continue;
      const content = frame.message?.content;
      if (!Array.isArray(content)) continue;
      for (const block of content) {
        const held = block as { type?: unknown; id?: unknown };
        if (held.type === 'tool_use' && typeof held.id === 'string') ids.add(held.id);
      }
    }
  }
  return ids;
}

describe('the chat fixture', () => {
  /**
   * **A card whose dispatch no canned turn holds would lead nowhere.** The
   * segment's list gates on reachability for exactly that reason, and with no
   * server behind the fixture the row could never arrive later - so the join
   * the fixture exists to show would be silently absent.
   */
  it('carries a card only for dispatches its turns hold', () => {
    const ids = dispatchIds();
    expect(ids.size, 'the turns carry dispatches at all').toBeGreaterThan(0);
    expect(fixture.cards, 'the fixture carries cards at all').not.toHaveLength(0);
    for (const card of fixture.cards) {
      expect(ids.has(card.dispatch_id), `the row behind ${card.dispatch_id}`).toBe(true);
    }
  });
});

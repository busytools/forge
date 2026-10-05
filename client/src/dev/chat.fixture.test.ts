import { describe, expect, it } from 'vitest';

import { reachableIds } from '../chat/subagents.svelte';
import fixture from './chat.fixture.json';

describe('the chat fixture', () => {
  /**
   * **A card whose dispatch no canned turn holds would lead nowhere.** The
   * segment's list gates on reachability for exactly that reason, and with no
   * server behind the fixture the row could never arrive later - so the join
   * the fixture exists to show would be silently absent.
   */
  it('carries a card only for dispatches its turns hold', () => {
    const ids = reachableIds(fixture.turns);
    expect(ids.size, 'the turns carry dispatches at all').toBeGreaterThan(0);
    expect(fixture.cards, 'the fixture carries cards at all').not.toHaveLength(0);
    for (const card of fixture.cards) {
      expect(ids.has(card.dispatch_id), `the row behind ${card.dispatch_id}`).toBe(true);
    }
  });
});

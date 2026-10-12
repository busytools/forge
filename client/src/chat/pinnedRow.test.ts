import { describe, expect, it } from 'vitest';

import { pinnedRow } from './pinnedRow.svelte';
import type { TurnInfo } from './units';

/** One running record, which is all this store ever carries. */
function info(over: Partial<TurnInfo> = {}): TurnInfo {
  return {
    running: true,
    failed: false,
    duration_ms: 1000,
    api_ms: null,
    ended_at_utc: null,
    model: null,
    thinking_tokens: null,
    input_tokens: null,
    output_tokens: null,
    cache_read_tokens: null,
    cache_written_tokens: null,
    session_cost_usd: null,
    ...over,
  };
}

describe('the pinned-row store', () => {
  it('answers nothing for a seat nothing has published', () => {
    expect(pinnedRow.of('session:nothing')).toBeNull();
  });

  it('hands back what a seat published, and forgets it when set back to null', () => {
    pinnedRow.set('session:one', info({ duration_ms: 4200 }));
    expect(pinnedRow.of('session:one')?.duration_ms).toBe(4200);
    pinnedRow.set('session:one', null);
    expect(pinnedRow.of('session:one')).toBeNull();
  });

  it('keeps seats apart', () => {
    pinnedRow.set('session:one', info());
    expect(pinnedRow.of('session:two')).toBeNull();
  });

  it('a later publish replaces the earlier record', () => {
    pinnedRow.set('session:one', info({ thinking_tokens: 10 }));
    pinnedRow.set('session:one', info({ thinking_tokens: 20 }));
    expect(pinnedRow.of('session:one')?.thinking_tokens).toBe(20);
  });
});

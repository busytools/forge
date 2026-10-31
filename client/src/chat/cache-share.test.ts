import { describe, expect, it } from 'vitest';

import { cached } from './report';
import type { TurnInfo } from './units';

/** A settled record with every counter present, which is the ordinary read. */
const FULL: TurnInfo = {
  running: false,
  failed: false,
  duration_ms: 1_000,
  api_ms: 500,
  ended_at_utc: '2026-10-31T10:00:00.000Z',
  model: 'claude-opus-5',
  thinking_tokens: 0,
  input_tokens: 4_000,
  output_tokens: 100,
  cache_read_tokens: 96_000,
  cache_written_tokens: 0,
  session_cost_usd: 1,
};

describe("a turn's cache share", () => {
  it('is the read over every input-side counter when all of them are present', () => {
    expect(cached(FULL), '96,000 of 100,000 read from the cache').toBe(96);
  });

  /**
   * **A missing counter is not a zero.** The share is over three counters and
   * each is independently optional on the wire, so a partial block has no
   * denominator to divide by - and a figure drawn from the missing ones read
   * as zero is a wrong number, where the column's convention is a dash.
   */
  it('is absent when the block carries a read but not every counter it divides by', () => {
    expect(
      cached({ ...FULL, input_tokens: null }),
      'a read with no input count cannot state a share of the input',
    ).toBeNull();
    expect(
      cached({ ...FULL, cache_written_tokens: null }),
      'a read with no cache-write count cannot either',
    ).toBeNull();
    expect(
      cached({ ...FULL, cache_read_tokens: null }),
      'and no read at all is a turn that never touched the cache',
    ).toBeNull();
  });

  it('keeps a real zero, which is a measurement rather than an absence', () => {
    expect(cached({ ...FULL, cache_read_tokens: 0 }), 'nothing served from the cache').toBe(0);
  });
});

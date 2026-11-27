import { describe, expect, it } from 'vitest';

import { ago, span } from './age';

describe('the shared relative age', () => {
  const now = 1_800_000_000;

  it('counts seconds under a minute and minutes under an hour', () => {
    expect(ago(now - 30, now), 'a fresh stamp reads in seconds').toBe('30s');
    expect(ago(now - 61, now)).toBe('1m');
    expect(ago(now - 3_599, now)).toBe('59m');
  });

  /**
   * **The days threshold belongs to the caller.** The board's own wording
   * counts hours however old the stamp is, and only the review's picker asks
   * for days - a shared helper that always read days would change the board
   * outside this page's cut.
   */
  it('keeps the board in hours, and reads days only when asked', () => {
    expect(ago(now - 3 * 86_400, now), 'the board still counts hours').toBe('72h');
    expect(ago(now - 3 * 86_400, now, 2 * 86_400), 'the review reads days').toBe('3d');
    expect(ago(now - 26 * 3_600, now, 2 * 86_400), 'a day-old stamp stays hours').toBe('26h');
  });

  /** A span a caller already holds - a quiet link's - reads in the same words. */
  it('reads a span directly', () => {
    expect(span(30)).toBe('30s');
    expect(span(61)).toBe('1m');
    expect(span(26 * 3_600)).toBe('26h');
    expect(span(3 * 86_400, 2 * 86_400)).toBe('3d');
  });
});

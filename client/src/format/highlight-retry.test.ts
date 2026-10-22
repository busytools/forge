import { describe, expect, it, vi } from 'vitest';

/** A grammar whose chunk fetch always fails, and how often it was asked for. */
const tries = vi.hoisted(() => ({ count: 0 }));

vi.mock('shiki/langs', () => ({
  bundledLanguages: {
    broken: (): Promise<never> => {
      tries.count += 1;
      return Promise.reject(new Error('the grammar chunk did not arrive'));
    },
  },
}));

import { highlighterFor } from './highlight';

describe('a grammar that fails to load', () => {
  /**
   * **A rejection must not be cached forever.** The load is de-duplicated
   * while it is in flight, so a second row opening the same language does
   * not fetch twice - but a promise left in the map after it rejected would
   * make every later open fail on the first chunk error. The second ask is
   * the retry.
   */
  it('is retried on the next open instead of the rejection being cached forever', async () => {
    await expect(highlighterFor('broken')).rejects.toThrow('did not arrive');
    await expect(highlighterFor('broken')).rejects.toThrow('did not arrive');
    expect(tries.count, 'the loader ran again on the second ask').toBe(2);
  });
});

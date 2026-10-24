import { describe, expect, it, vi } from 'vitest';

/** How often the engine was built, with the first build failing. */
const tries = vi.hoisted(() => ({ count: 0 }));

vi.mock('shiki/engine/oniguruma', async (importOriginal) => {
  const real = await importOriginal<typeof import('shiki/engine/oniguruma')>();
  return {
    createOnigurumaEngine: (...args: Parameters<typeof real.createOnigurumaEngine>) => {
      tries.count += 1;
      if (tries.count === 1) return Promise.reject(new Error('the wasm did not mount'));
      return real.createOnigurumaEngine(...args);
    },
  };
});

import { highlighterFor } from './highlight';

describe('an engine that fails to load', () => {
  /**
   * **A failed wasm load must not be cached forever.** The engine is built
   * once and shared, so a rejected promise left in `shared` would make every
   * later highlighter fail: chat panels plain and the Files reader saying it
   * could not be built, for the rest of the session. The second ask is the
   * retry, and it has to actually build again.
   */
  it('is rebuilt on the next ask instead of the rejection being cached forever', async () => {
    await expect(highlighterFor('rust')).rejects.toThrow('did not mount');

    const highlighter = await highlighterFor('rust');

    expect(tries.count, 'the engine was built again on the second ask').toBe(2);
    expect(highlighter.getLoadedLanguages(), 'and the second build is usable').toContain('rust');
  });
});

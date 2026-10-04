import { afterEach, describe, expect, it, vi } from 'vitest';

import { mintPromptId } from './ids';

/**
 * The id every prompt is sent under.
 *
 * **The property is uniqueness, not shape.** The CLI reads a repeated id as a
 * duplicate and drops that prompt with no error anywhere, so a fallback that
 * hands out the same id twice is worse than one that fails: the words go
 * missing silently. Each mint below is asserted against a fresh value, and
 * the fallbacks are driven by stubbing the API this platform would have had.
 */

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('minting a prompt id', () => {
  it('uses randomUUID where the platform has one', () => {
    vi.stubGlobal('crypto', { randomUUID: () => 'from-random-uuid' });

    expect(mintPromptId()).toBe('from-random-uuid');
  });

  it('falls back to getRandomValues on an insecure origin, where randomUUID is undefined', () => {
    // Exactly the LAN case: plain http, so `randomUUID` is absent while
    // `getRandomValues` is not.
    let calls = 0;
    vi.stubGlobal('crypto', {
      getRandomValues: (bytes: Uint8Array) => {
        calls += 1;
        bytes.fill(calls);
        return bytes;
      },
    });

    expect(mintPromptId()).toBe(`p-${'01'.repeat(16)}`);
    expect(mintPromptId(), 'the next mint draws again').toBe(`p-${'02'.repeat(16)}`);
  });

  it('mints unique ids with no entropy source at all', () => {
    vi.stubGlobal('crypto', undefined);

    const minted = new Set([mintPromptId(), mintPromptId(), mintPromptId()]);
    expect(minted.size, 'a repeat makes the CLI drop the prompt silently').toBe(3);
  });
});

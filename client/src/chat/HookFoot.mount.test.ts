// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import HookFoot from './HookFoot.svelte';
import type { HookFootLeaf } from './units';

/**
 * The turn foot's chip, mounted: the closed row says how many batches and
 * hooks it counts, and the open row lists every batch the reader would
 * otherwise lose (#3054 - the frames still draw, behind this row).
 *
 * **Only a mount can show the reader's own open**: SSR pins neither
 * `bind:open` nor the prop's default, so the chip is mounted closed (the
 * production default) and opened the way a reader opens it.
 */

const foot = (): HookFootLeaf => {
  const batch = (key: string, command: string) => ({
    key,
    actions: 2,
    infos: [{ command, durationMs: 3 }],
    errors: [],
    hasOutput: false,
    additionalContext: [],
    preventedContinuation: false,
  });
  return {
    key: 'uuid_7#hookbatch#hookfoot',
    batches: [
      batch('uuid_7#hookbatch', 'echo fixture-stop-hook-1'),
      batch('uuid_8#hookbatch', 'echo fixture-stop-hook-2'),
    ],
    actions: 4,
  };
};

describe("the turn foot's chip, mounted", () => {
  it('draws its counts while closed and every batch onto the open', () => {
    const app = mount(HookFoot, { target: document.body, props: { foot: foot() } });
    try {
      flushSync();
      const summary = document.body.textContent ?? '';
      expect(summary, 'the batches and the hooks it counts').toContain(
        'hooks \u{b7} 2 batches \u{b7} 4 hooks',
      );
      expect(summary, 'closed, the bodies wait behind the row').not.toContain(
        'echo fixture-stop-hook-1',
      );

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the chip drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();

      const open = document.body.textContent ?? '';
      expect(open, 'every batch the chip counted is listed').toContain('echo fixture-stop-hook-1');
      expect(open, 'both of them, in arrival order').toContain('echo fixture-stop-hook-2');
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});
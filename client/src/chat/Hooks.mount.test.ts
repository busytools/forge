// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Hooks from './Hooks.svelte';
import type { HookInfo } from './units';

/**
 * The chip's own open, as MOUNTED DOM.
 *
 * **A closed chip carries its summary and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the chip is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

describe('the hook chip, mounted', () => {
  it("draws its hooks onto the reader's open, and holds nothing while closed", () => {
    const app = mount(Hooks, {
      target: document.body,
      props: { actions: 1, infos: ONE, errors: [] },
    });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, nothing under the summary').not.toContain(
        'echo fixture-stop-hook-ok',
      );

      const chip = document.querySelector('details');
      if (!(chip instanceof HTMLDetailsElement)) throw new Error('the chip drew no details');
      chip.open = true;
      chip.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelector('.term')?.textContent ?? '', 'the hooks draw').toContain(
        'echo fixture-stop-hook-ok',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});

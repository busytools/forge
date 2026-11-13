// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Hooks from './Hooks.svelte';
import type { HookInfo } from './units';

/**
 * The row's own open, as MOUNTED DOM.
 *
 * **A closed row carries its summary and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the row is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
const ONE: HookInfo[] = [{ command: 'echo fixture-stop-hook-ok', durationMs: 3 }];

describe('the hook batch row, mounted', () => {
  it("draws its hooks onto the reader's open, and holds nothing while closed", () => {
    const app = mount(Hooks, {
      target: document.body,
      props: { batch: { key: 'uuid_7#hookbatch', actions: 1, infos: ONE, errors: [] } },
    });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, nothing under the summary').not.toContain(
        'echo fixture-stop-hook-ok',
      );

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
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

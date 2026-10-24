// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import HookGroup from './HookGroup.svelte';
import type { HookGroupLeaf } from './units';

/**
 * The start's own open, as MOUNTED DOM.
 *
 * SSR pins neither `bind:open` nor the prop's default, so the row is mounted
 * closed - the production state a fresh session draws - and then opened the
 * way a reader opens it.
 */
const START: HookGroupLeaf = {
  key: 'hook-0',
  event: 'SessionStart',
  runs: [
    {
      key: 'hook-0',
      run: {
        name: 'SessionStart:startup',
        event: null,
        failed: false,
        settled: true,
        body: 'repository is clean\n',
      },
    },
    {
      key: 'hook-1',
      run: { name: 'SessionStart:startup', event: null, failed: false, settled: true, body: null },
    },
  ],
};

describe('the session start row, mounted', () => {
  it("draws its hooks onto the reader's open, and holds nothing while closed", () => {
    const app = mount(HookGroup, { target: document.body, props: { group: START } });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, nothing under the summary').not.toContain(
        'repository is clean',
      );

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      const hooks = [...document.querySelectorAll('details.hookrow')];
      expect(hooks, 'one row per hook that said something').toHaveLength(1);
      expect(hooks[0]?.textContent ?? '', "the hook's own words on its closed row").toContain(
        'repository is clean',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});

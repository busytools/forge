// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Hook from './Hook.svelte';
import type { HookRun } from './units';

/**
 * The row's own open, as MOUNTED DOM.
 *
 * **A closed row carries its summary and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the row is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
const run: HookRun = {
  name: 'SessionStart:startup',
  event: null,
  failed: false,
  body: 'capture-line-1\ncapture-line-2\n',
};

describe('the hook row, mounted', () => {
  it("draws its body onto the reader's open, and holds nothing while closed", () => {
    const app = mount(Hook, { target: document.body, props: { run } });
    try {
      flushSync();
      // The summary's own tail joins the body, so the absence to read is the
      // body's markup, not its words.
      expect(document.querySelector('.term'), 'closed, no body under the summary').toBeNull();

      const row = document.querySelector('details');
      if (!(row instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      row.open = true;
      row.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelector('.term')?.textContent ?? '', 'the body draws').toContain(
        'capture-line-1',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});

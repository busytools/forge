// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import CompactionPoint from './CompactionPoint.svelte';

/**
 * The point's own open, as MOUNTED DOM.
 *
 * **A closed point carries its summary and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the point is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
describe('the compaction point, mounted', () => {
  it("draws its account onto the reader's open, and holds nothing while closed", () => {
    const app = mount(CompactionPoint, {
      target: document.body,
      props: {
        trigger: 'auto',
        preTokens: 12_000,
        postTokens: 3_000,
        summary: 'the compacted account',
      },
    });
    try {
      flushSync();
      expect(document.body.textContent ?? '', 'closed, nothing under the summary').not.toContain(
        'the compacted account',
      );

      const point = document.querySelector('details');
      if (!(point instanceof HTMLDetailsElement)) throw new Error('the point drew no details');
      point.open = true;
      point.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelector('.cpbody')?.textContent ?? '', 'the account draws').toContain(
        'the compacted account',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});

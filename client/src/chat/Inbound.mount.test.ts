// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { describe, expect, it } from 'vitest';

import Inbound from './Inbound.svelte';
import type { InboundLeaf } from './units';

/**
 * The delivery row's own open, as MOUNTED DOM.
 *
 * **A closed row carries its summary and nothing else, and only a mount can
 * show the reader's own open**: SSR pins neither `bind:open` nor the prop's
 * default, so the row is mounted closed (the production default) and then
 * opened the way a reader opens it.
 */
const row: InboundLeaf = {
  key: 'delivery-1',
  kind: 'cron',
  title: 'the morning sweep',
  body: 'the payload the body alone carries',
  elevated: false,
};

describe('the inbound delivery row, mounted', () => {
  it("draws its body onto the reader's open, and holds nothing while closed", () => {
    const app = mount(Inbound, { target: document.body, props: { row } });
    try {
      flushSync();
      // The summary's own tail carries the body's line, so the absence to
      // read is the body's markup, not its words.
      expect(document.querySelector('.body'), 'closed, no body under the summary').toBeNull();

      const drawn = document.querySelector('details');
      if (!(drawn instanceof HTMLDetailsElement)) throw new Error('the row drew no details');
      drawn.open = true;
      drawn.dispatchEvent(new Event('toggle'));
      flushSync();
      expect(document.querySelector('.body')?.textContent ?? '', 'the body draws').toContain(
        'the payload the body alone carries',
      );
    } finally {
      void unmount(app);
      document.body.innerHTML = '';
    }
  });
});

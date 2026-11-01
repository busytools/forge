// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import LiveRow from './LiveRow.svelte';

/**
 * The browser chip, which no store seeds on its own.
 *
 * **The browser row reads the server**, so nothing a test syncs can make it
 * draw: the chip comes from a tab listing the link hands over. Without this
 * case `<BrowserSegment>` could be deleted from the row and the whole suite
 * would stay green.
 */
const LIST = vi.hoisted(() => ({
  profile: 'Busytools/default',
  id: 1,
  up: true,
  connected: true,
  closing_at_ms: null,
  rows: [
    {
      number: 1,
      title: 'the deploy page',
      url: 'https://example.org/',
      owner: { org: 'Busytools', project: 'forge', label: 'lead' },
      state: 'in_use',
    },
  ],
}));

vi.mock('../browser/link', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/link')>();
  return {
    ...actual,
    canAttach: (): boolean => true,
    drawsSurface: (): boolean => false,
    drawsCanvas: (): boolean => false,
    listTabs: () => Promise.resolve(LIST),
    onTabs: () => Promise.resolve(() => {}),
    onConnection: () => Promise.resolve(() => {}),
  };
});

const SLOT = { org: 'Busytools', project: 'forge', label: 'lead' };

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the live row and the browser', () => {
  it('draws the browser chip from the server listing, first in the order', async () => {
    // The draft chip gives the row a reason to draw, so the browser segment -
    // which is mounted with the row - mounts too.
    app = mount(LiveRow, {
      target: document.body,
      props: { info: null, slot: SLOT, draft: { lines: 1 } },
    });
    flushSync();
    // The listing is a promise; let it land.
    await vi.waitFor(() => {
      flushSync();
      expect(document.querySelector('[data-chip="browser"]')?.firstElementChild).not.toBeNull();
    });

    const order = [...document.querySelectorAll<HTMLElement>('[data-chip]')]
      .filter((el) => el.firstElementChild !== null)
      .map((el) => el.dataset.chip);
    expect(order[0], 'the browser chip is not the row order it is drawn in').toBe('browser');
  });
});

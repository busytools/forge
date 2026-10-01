// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { clear, list } from './records';
import Swapped from './Swapped.svelte';

/** Let the list mount and every deferred pass land, the way `follow.test.ts` does. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  flushSync();
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  clear();
});

/**
 * The seam a column reads, and what a list leaving it must NOT take with it.
 *
 * A column drives its reader through `list()`, and every drive is `list()?.…`,
 * so a handle cleared while a list is still on screen leaves a test that reads
 * green while driving nothing.
 */
describe('the handle a list registers at the seam', () => {
  it('belongs to the list on screen after a replacement swap in one flush', async () => {
    const swapped = writable(false);
    app = mount(Swapped, { target: document.body, props: { swapped } });
    await settle();
    expect(list(), 'the first list registered itself as it mounted').not.toBeNull();
    const first = list();

    swapped.set(true);
    flushSync();

    expect(
      document.querySelectorAll('.conv'),
      'the swap left exactly the replacement list on screen',
    ).toHaveLength(1);
    expect(list(), 'the seam still holds a handle with a list on screen').not.toBeNull();
    expect(
      list(),
      'the handle belongs to the replacement list, not to the one that departed',
    ).not.toBe(first);
  });
});

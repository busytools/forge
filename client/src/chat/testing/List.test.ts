// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { clear, list, register } from './records';
import List from './List.svelte';
import Siblings from './Siblings.svelte';
import Swapped from './Swapped.svelte';
import { installResizeObserver } from './viewport';

// The stub's items report their sizes through an observer, which jsdom has
// none of.
installResizeObserver();

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

  it('leaves the surviving list holding the seam when its neighbour departs', () => {
    // Where a departed list leaves the seam, which is the state this starts at.
    register(null);
    const both = writable(true);
    app = mount(Siblings, { target: document.body, props: { both } });
    flushSync();
    const survivor = list();
    expect(survivor, 'two lists mounted, the last one at the seam').not.toBeNull();

    both.set(false);
    flushSync();

    expect(
      document.querySelectorAll('.conv'),
      'the departing list left the survivor on screen',
    ).toHaveLength(1);
    expect(list(), 'the departing list does not clear a handle that is not its own').toBe(survivor);
  });

  it('holds nothing for a list that was unmounted before it ever flushed', () => {
    // Where a departed list leaves the seam, which is the state this starts at.
    register(null);
    app = mount(List, { target: document.body });
    expect(document.querySelector('.conv'), 'the list rendered before the unmount').not.toBeNull();
    // No flush between the two, so the effect never ran.
    void unmount(app);
    app = null;

    expect(
      list(),
      'a list unmounted before its effect ran leaves no handle at the seam',
    ).toBeNull();
  });
});

/**
 * **The base an item is given is the negative the drawn-row tests rest on.**
 * virtua's ListItem lands its own style first - an unmeasured row is
 * `visibility: hidden` - and the item's style after it; the stub mirrors
 * that, so a column that overrides nothing leaves a row a browser hides.
 */
describe('the style an item is given', () => {
  it('hides a row until it is measured, and lets an item override it', () => {
    const data = [{ key: 'a' }];
    app = mount(List, {
      target: document.body,
      props: { data, getKey: (row: unknown) => (row as { key: string }).key },
    });
    flushSync();
    const plain = document.querySelector<HTMLElement>('.conv > .turn');
    expect(plain?.style.visibility, 'no item props, the base hides the row').toBe('hidden');

    void unmount(app);
    app = null;
    document.body.innerHTML = '';
    app = mount(List, {
      target: document.body,
      props: {
        data,
        getKey: (row: unknown) => (row as { key: string }).key,
        itemProps: () => ({ style: { visibility: 'visible' } }),
      },
    });
    flushSync();
    const overridden = document.querySelector<HTMLElement>('.conv > .turn');
    expect(overridden?.style.visibility, 'an item style rides after the base').toBe('visible');
  });
});

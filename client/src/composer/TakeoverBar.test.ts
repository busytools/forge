// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { selectTab, surface } from '../browser/surface.svelte';
import TakeoverBar from './TakeoverBar.svelte';

vi.mock('../browser/surface.svelte', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../browser/surface.svelte')>();
  return { ...actual, selectTab: vi.fn(() => Promise.resolve(null)) };
});

let shown: ReturnType<typeof mount> | null = null;

/**
 * Take the mounted bar down and wait for it: a mount that starts before the
 * previous component finished unmounting is how two bars' handlers end up on
 * one click.
 */
async function stop(): Promise<void> {
  if (shown !== null) await unmount(shown);
  shown = null;
  document.body.innerHTML = '';
  surface.viewer = null;
  surface.controller = null;
  surface.tabs = null;
}

/** One bar, mounted for one test; never re-mounted inside a test. The
 *  surface holds the controller role, so the arrows are live unless a test
 *  takes the role away. */
async function bar(
  nav: { can_back: boolean; can_forward: boolean },
  onnav: (direction: 'back' | 'forward') => void,
  holding = true,
): Promise<HTMLElement> {
  await stop();
  surface.viewer = 1;
  surface.controller = holding ? 1 : null;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(TakeoverBar, {
    target,
    props: { title: 'browser hand-off', onback: () => {}, nav, onnav },
  });
  flushSync();
  return target;
}

function arrow(target: HTMLElement, name: 'back' | 'forward'): HTMLButtonElement {
  const button = target.querySelector<HTMLButtonElement>(`[aria-label="${name}"]`);
  if (button === null) throw new Error(`the bar draws no ${name} arrow`);
  return button;
}

afterEach(async () => {
  await stop();
});

describe("the takeover bar's history arrows", () => {
  it('draws both arrows disabled on a fresh page', async () => {
    const target = await bar({ can_back: false, can_forward: false }, () => {});
    expect(arrow(target, 'back').disabled, 'a fresh page goes nowhere back').toBe(true);
    expect(arrow(target, 'forward').disabled, 'a fresh page goes nowhere forward').toBe(true);
  });

  it('opens back after a navigation, with forward still disabled', async () => {
    const target = await bar({ can_back: true, can_forward: false }, () => {});
    expect(arrow(target, 'back').disabled, 'a link was followed: back is open').toBe(false);
    expect(arrow(target, 'forward').disabled, 'forward has nowhere to go yet').toBe(true);
  });

  it('opens forward after going back', async () => {
    const target = await bar({ can_back: true, can_forward: true }, () => {});
    expect(arrow(target, 'back').disabled, 'back after going back is still open').toBe(false);
    expect(arrow(target, 'forward').disabled, 'went back: forward is open').toBe(false);
  });

  it('sends back from the back arrow, and nothing from a disabled forward', async () => {
    const onnav = vi.fn();
    const target = await bar({ can_back: true, can_forward: false }, onnav);
    arrow(target, 'back').click();
    expect(onnav).toHaveBeenCalledWith('back');
    arrow(target, 'forward').click();
    expect(onnav, 'a disabled arrow asks for nothing').toHaveBeenCalledTimes(1);
  });

  it('sends forward from the forward arrow', async () => {
    const onnav = vi.fn();
    const target = await bar({ can_back: true, can_forward: true }, onnav);
    arrow(target, 'forward').click();
    expect(onnav).toHaveBeenCalledWith('forward');
  });

  /// **A watcher's arrows are dead even on a page with history**: the server
  /// refuses the move, and a live-looking arrow would send a gesture the
  /// person can see does nothing.
  it('draws both arrows disabled for a viewer that does not hold the role', async () => {
    const target = await bar({ can_back: true, can_forward: true }, () => {}, false);
    expect(arrow(target, 'back').disabled, 'a watcher moves nothing').toBe(true);
    expect(arrow(target, 'forward').disabled, 'and nothing forward either').toBe(true);
  });
});

describe("the takeover bar's tab picker", () => {
  /// **Each option names the tab, its page and its owner**, so the person
  /// picking knows whose agent the tab belongs to, and a switch re-attaches
  /// to that tab.
  it('lists the profile tabs with their owner and switches on a pick', async () => {
    await stop();
    surface.viewer = 1;
    surface.controller = 1;
    surface.tab = 2;
    surface.tabs = {
      profile: 'Busytools/work',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [
        {
          number: 2,
          title: 'org tabs fixture',
          url: 'http://127.0.0.1:8799/',
          owner: { org: 'Busytools', project: 'forge', label: 'lead' },
          state: 'idle',
        },
        {
          number: 3,
          title: '',
          url: 'https://example.com/',
          owner: { org: 'Busytools', project: 'forge', label: 'w1' },
          state: 'in_use',
        },
      ],
    };
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(TakeoverBar, {
      target,
      props: { title: 'browser hand-off', onback: () => {} },
    });
    flushSync();

    const picker = target.querySelector<HTMLSelectElement>('.tb-pick');
    expect(picker, 'the bar draws the picker').not.toBeNull();
    const options = [...(picker?.options ?? [])].map((option) => option.text);
    expect(options[0]).toContain('2');
    expect(options[0]).toContain('org tabs fixture');
    expect(options[0]).toContain('Busytools/forge/lead');
    expect(options[1]).toContain('https://example.com/');
    expect(options[1]).toContain('Busytools/forge/w1');

    if (picker !== null) {
      picker.value = '3';
      picker.dispatchEvent(new Event('change', { bubbles: true }));
    }
    expect(selectTab).toHaveBeenCalledWith(3);
  });
});

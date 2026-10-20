// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import TakeoverBar from './TakeoverBar.svelte';

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
}

/** One bar, mounted for one test; never re-mounted inside a test. */
async function bar(
  nav: { can_back: boolean; can_forward: boolean },
  onnav: (direction: 'back' | 'forward') => void,
): Promise<HTMLElement> {
  await stop();
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
});

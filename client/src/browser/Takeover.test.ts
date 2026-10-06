// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { takeover } from './takeover.svelte';
import TakeoverScreen from './TakeoverScreen.svelte';

vi.mock('./host', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./host')>();
  return {
    ...actual,
    openTakeover: vi.fn(() => Promise.resolve()),
    closeTakeover: vi.fn(() => Promise.resolve()),
    takeoverActive: vi.fn(() => Promise.resolve(false)),
  };
});

import { closeTakeover, openTakeover, takeoverActive } from './host';

function draw(address = '127.0.0.1:8790') {
  const target = document.createElement('div');
  document.body.append(target);
  const app = mount(TakeoverScreen, { target, props: { address } });
  flushSync();
  return { target, stop: () => void unmount(app) };
}

const click = (el: Element | null | undefined): void => {
  if (!(el instanceof HTMLElement)) throw new Error('nothing to click');
  el.click();
  flushSync();
};

afterEach(() => {
  vi.mocked(openTakeover).mockClear();
  vi.mocked(closeTakeover).mockClear();
  takeover.active = false;
  takeover.asking = null;
});

describe('the takeover', () => {
  it('draws the bar with the address, and Done only while a hand-off is held', () => {
    takeover.active = true;
    const shown = draw('127.0.0.1:8792');

    expect(shown.target.textContent, 'the way back is named').toContain('back to forge');
    expect(shown.target.textContent, 'and the address rides the bar').toContain('127.0.0.1:8792');
    expect(shown.target.textContent, 'nothing to answer yet').not.toContain('Done');

    takeover.asking = { id: 'h1', done: () => {} };
    const again = draw();
    expect(again.target.textContent, 'a held hand-off brings Done').toContain('Done');
    shown.stop();
    again.stop();
  });

  it('backs out: the view comes down and the screen returns', () => {
    takeover.active = true;
    const shown = draw();

    click(shown.target.querySelector('.back'));

    expect(closeTakeover, 'the shell is told to take the view down').toHaveBeenCalledTimes(1);
    expect(takeover.active, 'and the screen is no longer the browser').toBe(false);
    shown.stop();
  });

  it('answers the held hand-off from the bar, then backs out', () => {
    takeover.active = true;
    const done = vi.fn();
    takeover.asking = { id: 'h1', done };
    const shown = draw();

    click(shown.target.querySelector('.done'));

    expect(done, 'the same answer the dock gives').toHaveBeenCalledTimes(1);
    expect(takeover.asking, 'and the bar no longer offers it').toBeNull();
    expect(closeTakeover).toHaveBeenCalledTimes(1);
    shown.stop();
  });

  it('reads Escape as the way back', () => {
    takeover.active = true;
    const shown = draw();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    flushSync();

    expect(closeTakeover, 'Escape is the same door as the button').toHaveBeenCalledTimes(1);
    expect(takeover.active).toBe(false);
    shown.stop();
  });

  it('opens only when the engine answered, and says so through the store', async () => {
    vi.mocked(openTakeover).mockRejectedValueOnce(new Error('no engine'));
    await expect(takeover.open()).rejects.toThrow('no engine');
    expect(takeover.active, 'a view that never came up is not on screen').toBe(false);

    await takeover.open();
    expect(takeover.active, 'an engine that answered is').toBe(true);
    expect(openTakeover).toHaveBeenCalledWith(44);
  });

  it('re-draws the screen a reloaded window was on', async () => {
    vi.mocked(takeoverActive).mockResolvedValueOnce(true);
    await takeover.sync();
    expect(takeover.active, 'the shell still held the view').toBe(true);
  });
});

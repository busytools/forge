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
    takeoverInput: vi.fn(() => Promise.resolve()),
    onTakeoverFrame: vi.fn(() => Promise.resolve(() => {})),
  };
});

import {
  closeTakeover,
  onTakeoverFrame,
  openTakeover,
  takeoverActive,
  takeoverInput,
} from './host';

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

  /** A frame lands on the canvas at the PAGE's own pixel size, and the events
   *  that follow go back down the same connection in the page's coordinates. */
  async function withFrame(): Promise<{ target: HTMLElement; frame: (size: number) => void }> {
    let deliver: ((frame: { data: string; width: number; height: number }) => void) | null = null;
    vi.mocked(onTakeoverFrame).mockImplementationOnce((fn) => {
      deliver = fn;
      return Promise.resolve(() => {});
    });
    takeover.active = true;
    const shown = draw();
    await vi.waitFor(() => expect(deliver).not.toBeNull());
    return {
      target: shown.target,
      frame: (size: number) => {
        deliver?.({ data: 'aGk=', width: size, height: size });
        flushSync();
      },
    };
  }

  it("draws a frame at the page's own pixel size", async () => {
    const shown = await withFrame();
    shown.frame(800);

    const canvas = shown.target.querySelector('canvas');
    expect(canvas?.width, 'the backing store is the page').toBe(800);
    expect(canvas?.height).toBe(800);
  });

  it("forwards a click in the page's coordinates, and typing too", async () => {
    const shown = await withFrame();
    shown.frame(800);
    const canvas = shown.target.querySelector('canvas');
    if (!(canvas instanceof HTMLCanvasElement)) throw new Error('no canvas');
    // jsdom lays nothing out; the drawn box is the page scaled into a stage.
    vi.spyOn(canvas, 'getBoundingClientRect').mockReturnValue({
      left: 20,
      top: 60,
      width: 400,
      height: 400,
      right: 420,
      bottom: 460,
      x: 20,
      y: 60,
      toJSON: () => ({}),
    });

    canvas.dispatchEvent(
      new PointerEvent('pointerdown', { clientX: 220, clientY: 260, bubbles: true }),
    );
    expect(takeoverInput, 'the click maps to the page, not the screen').toHaveBeenLastCalledWith(
      'Input.dispatchMouseEvent',
      { type: 'mousePressed', x: 400, y: 400, buttons: 1, button: 'left', clickCount: 1 },
    );

    canvas.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', code: 'KeyA', bubbles: true }));
    expect(takeoverInput, 'a key goes down the same way').toHaveBeenLastCalledWith(
      'Input.dispatchKeyEvent',
      { type: 'keyDown', key: 'a', code: 'KeyA', modifiers: 0, text: 'a' },
    );

    const before = vi.mocked(takeoverInput).mock.calls.length;
    canvas.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(
      vi.mocked(takeoverInput).mock.calls.length,
      "Escape is the way back, not the page's key",
    ).toBe(before);
  });
});

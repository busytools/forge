// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * The phone's picture: no native overlay exists there, so the page paints
 * each relayed frame on a canvas and sends the person's gestures back. The
 * desktop's arm (the shell's overlay) is not this test's.
 */
vi.mock('./host', () => ({
  canHost: vi.fn(() => true),
  drawsSurface: vi.fn(() => true),
  drawsCanvas: vi.fn(() => true),
  keptFrame: vi.fn(() => Promise.resolve(null)),
  showBrowser: vi.fn(() => Promise.resolve(null)),
  hideBrowser: vi.fn(() => Promise.resolve({ lowered: true })),
  listTabs: vi.fn(() =>
    Promise.resolve({
      profile: 'hunt',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [],
    }),
  ),
  tabListOf: (value: unknown) => value,
  reportSurfaceRect: vi.fn(() => Promise.resolve()),
  resizeBrowser: vi.fn(() => Promise.resolve()),
  takeControl: vi.fn(() => Promise.resolve()),
}));

import BrowserSurface from './BrowserSurface.svelte';
import { keptFrame } from './host';
import { setRegion, surface } from './surface.svelte';
import type { BrowserFrame, Connection } from '../socket';
import type { BrowserInputEvent } from '../protocol';

/** The frames and gestures a stubbed connection carried. */
function stubConnection() {
  const listeners = new Set<(frame: BrowserFrame) => void>();
  const events: BrowserInputEvent[] = [];
  const connection = {
    onBrowserFrame: (fn: (frame: BrowserFrame) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    browserInput: (event: BrowserInputEvent) => {
      events.push(event);
    },
  } as unknown as Connection;
  return { connection, listeners, events };
}

let shown: ReturnType<typeof mount> | null = null;

function open(connection: Connection): HTMLElement {
  surface.phase = 'live';
  surface.reason = null;
  surface.profile = null;
  surface.tab = 2;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(BrowserSurface, { target, props: { connection } });
  flushSync();
  return target;
}

/** One decoded frame, as the shell's event carries it. */
function frame(seq: number): BrowserFrame {
  return {
    profile: 1,
    tab: 2,
    seq,
    width: 100,
    height: 50,
    key: seq === 0,
    bytes: new Uint8Array([1]),
  };
}

beforeEach(() => {
  surface.phase = 'closed';
  surface.id = null;
  surface.tab = null;
  surface.viewer = null;
  surface.controller = null;
  vi.mocked(keptFrame).mockReset();
  vi.mocked(keptFrame).mockResolvedValue(null);
  // jsdom has no 2d context and prints a notice per paint; the frame's SIZE
  // is this test's subject, not the pixels.
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null);
});

afterEach(() => {
  if (shown !== null) void unmount(shown);
  shown = null;
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

describe('the canvas viewer', () => {
  /// **A surface that mounts late paints the frame it missed.** In
  /// production the component mounts at `opening` and the canvas appears
  /// only when the attach's reply lands as `live`; the kept frame is asked
  /// for at that point, not at the first mount.
  it('paints the kept frame once the canvas is live', async () => {
    // **The ask waits for the canvas, and the answer is held open** until
    // after the flip to live: a test whose mock resolves early cannot tell
    // the guarded effect from an unguarded one.
    let release: (frame: BrowserFrame | null) => void = () => {};
    vi.mocked(keptFrame).mockReturnValue(
      new Promise<BrowserFrame | null>((resolve) => {
        release = resolve;
      }),
    );
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 320, height: 200, close: () => {} }),
    );
    const { connection } = stubConnection();
    // The production order: opening first, with no canvas in the DOM.
    surface.phase = 'opening';
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(BrowserSurface, { target, props: { connection } });
    flushSync();
    expect(target.querySelector('canvas'), 'no canvas while opening').toBeNull();
    expect(keptFrame, 'no ask while the canvas does not exist').not.toHaveBeenCalled();

    surface.phase = 'live';
    // The same `browser_view` that flips the surface live names its id and tab.
    surface.id = 1;
    surface.tab = 2;
    flushSync();
    await vi.waitFor(() => expect(keptFrame).toHaveBeenCalledTimes(1));
    const canvas = target.querySelector('canvas');
    release({
      profile: 1,
      tab: 2,
      seq: 4,
      width: 320,
      height: 200,
      key: true,
      bytes: new Uint8Array([1]),
    });
    await vi.waitFor(() => expect(canvas?.width).toBe(320));
    expect(canvas?.height).toBe(200);
  });

  /// **A relayed frame becomes the picture.** The canvas takes the frame's
  /// own size (the page's viewport), and the bitstream is decoded as WebP.
  it('paints a relayed frame on the canvas at its own size', async () => {
    const decoded: Blob[] = [];
    vi.stubGlobal('createImageBitmap', (blob: Blob) => {
      decoded.push(blob);
      return Promise.resolve({ width: 100, height: 50, close: () => {} });
    });
    const { connection, listeners } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    expect(canvas, 'the canvas is the page area').not.toBeNull();
    surface.id = 1;
    flushSync();

    for (const listener of listeners) listener(frame(0));
    await vi.waitFor(() => expect(canvas?.width).toBe(100));

    expect(canvas?.height).toBe(50);
    expect(decoded).toHaveLength(1);
    expect(decoded[0]?.type, 'the frame is a WebP still').toBe('image/webp');
  });

  /// **Only the viewed profile's frames paint.** The server relays one
  /// profile per connection, so a frame for another is one that raced the
  /// attach - drawing it would show the wrong page.
  it('drops a frame for another profile', async () => {
    const decoded: Blob[] = [];
    vi.stubGlobal('createImageBitmap', (blob: Blob) => {
      decoded.push(blob);
      return Promise.resolve({ width: 100, height: 50, close: () => {} });
    });
    const { connection, listeners } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 2;
    flushSync();

    for (const listener of listeners) listener(frame(0));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(decoded, "another profile's frame is not decoded").toHaveLength(0);
    expect(canvas.width, 'and the canvas keeps its own size').toBe(300);

    surface.id = 1;
    flushSync();
    for (const listener of listeners) listener(frame(1));
    await vi.waitFor(() => expect(canvas.width).toBe(100));
    expect(decoded).toHaveLength(1);
  });

  /// **Only the viewed tab's frames paint.** The profile can have several
  /// tabs; a frame for one the person is not looking at is another page's
  /// picture, and drawing it would be the wrong page under this one's name.
  it('drops a frame for another tab', async () => {
    const decoded: Blob[] = [];
    vi.stubGlobal('createImageBitmap', (blob: Blob) => {
      decoded.push(blob);
      return Promise.resolve({ width: 100, height: 50, close: () => {} });
    });
    const { connection, listeners } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    surface.tab = 2;
    flushSync();

    for (const listener of listeners) listener({ ...frame(0), tab: 3 });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(decoded, "another tab's frame is not decoded").toHaveLength(0);
    expect(canvas.width, 'and the canvas keeps its own size').toBe(300);

    for (const listener of listeners) listener(frame(1));
    await vi.waitFor(() => expect(canvas.width).toBe(100));
    expect(decoded).toHaveLength(1);
  });

  /// **A frame that lands before the attach names its profile does not
  /// paint.** After a re-attach the previous profile's frame can arrive
  /// before the new `browser_view`; with a null id it painted under the new
  /// surface's name. The kept-frame ask runs after that same `browser_view`.
  it('drops a frame that lands before the attach names its profile', async () => {
    const decoded: Blob[] = [];
    vi.stubGlobal('createImageBitmap', (blob: Blob) => {
      decoded.push(blob);
      return Promise.resolve({ width: 100, height: 50, close: () => {} });
    });
    const { connection, listeners } = stubConnection();
    open(connection);
    expect(surface.id, 'no id before the attach').toBeNull();
    for (const listener of listeners) listener(frame(0));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(decoded, 'the unattributed frame is not decoded').toHaveLength(0);
  });

  /// **A gesture crosses in the PAGE's CSS pixels, mapped through the
  /// picture's letterboxed box.** The canvas is scaled to fit, so its element
  /// is not always the page's size; reading the client point straight put
  /// taps off the field they were aimed at. The buffer's device pixels are
  /// not the page's either.
  it("maps the picture's point through the page area, not the buffer's device pixels", async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 400, height: 200, close: () => {} }),
    );
    const { connection, listeners, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    flushSync();
    for (const listener of listeners) listener({ ...frame(0), width: 400, height: 200 });
    await vi.waitFor(() => expect(canvas.width).toBe(400));
    // A 145x73 picture showing a 290x146 page: every point doubles.
    vi.spyOn(canvas, 'getBoundingClientRect').mockReturnValue({
      x: 0,
      y: 0,
      left: 0,
      top: 0,
      right: 145,
      bottom: 73,
      width: 145,
      height: 73,
      toJSON: () => ({}),
    });
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();
    // After the role settles: taking it re-measures, and jsdom's own page
    // area is zero.
    setRegion({ x: 0, y: 0, width: 290, height: 146 }, { width: 390, height: 844 }, 1, 1);

    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 10, clientY: 5, bubbles: true }));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ kind: 'mouse', action: 'press', x: 20, y: 10 });
  });

  /// **Only the controller's gestures go out.** A surface that does not hold
  /// the role draws the page and sends nothing - the server would refuse it,
  /// and the bar's Take control is the door.
  it('sends a gesture only while this surface holds the role', () => {
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    expect(events, 'a surface without the role sends nothing').toHaveLength(0);

    surface.viewer = 7;
    surface.controller = 7;
    flushSync();
    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ kind: 'mouse', action: 'press', button: 0 });
  });

  /** A pointer event of the kind a finger sends. */
  function finger(type: string, x: number, y: number): Event {
    const event = new MouseEvent(type, { clientX: x, clientY: y, bubbles: true });
    Object.defineProperty(event, 'pointerType', { value: 'touch' });
    Object.defineProperty(event, 'pointerId', { value: 3 });
    return event;
  }

  /// **A tap is a click and a swipe is a scroll.** A finger's drag sent as a
  /// held mouse button selected text and dragged links; the phone maps
  /// vertical motion to the wheel instead, and a still tap to press+release.
  it('sends a tap as a click and a swipe as a wheel', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(finger('pointerdown', 10, 60));
    canvas.dispatchEvent(finger('pointermove', 10, 48));
    canvas.dispatchEvent(finger('pointermove', 10, 30));
    canvas.dispatchEvent(finger('pointerup', 10, 30));
    expect(
      events.filter((event) => event.kind === 'wheel'),
      'the swipe scrolls',
    ).not.toHaveLength(0);
    expect(
      events.filter((event) => event.kind === 'mouse'),
      'and never drags',
    ).toHaveLength(0);
    const wheel = events.find((event) => event.kind === 'wheel');
    if (wheel?.kind !== 'wheel') throw new Error('the swipe sent no wheel');
    expect(wheel.delta_y, 'a finger moving up scrolls down').toBeGreaterThan(0);

    events.length = 0;
    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    expect(events, 'a still tap is a press and its release').toMatchObject([
      { kind: 'mouse', action: 'press' },
      { kind: 'mouse', action: 'release' },
    ]);
  });

  /// **The phone's keys and text reach the page.** The canvas has no fields,
  /// so a hidden one takes the keyboard while this surface holds control;
  /// named keys cross as key events and inserted text whole.
  it('sends keys and typed text from the hidden field', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('no keyboard field');
    // A tap on the page is what raises the phone's keyboard.
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    expect(document.activeElement, 'the field holds the keyboard').toBe(input);
    events.length = 0; // the tap's own click

    const enter = new KeyboardEvent('keydown', {
      key: 'Enter',
      code: 'Enter',
      bubbles: true,
    });
    Object.defineProperty(enter, 'keyCode', { value: 13 });
    input.dispatchEvent(enter);
    expect(events, 'a named key crosses down and up with its virtual key code').toMatchObject([
      { kind: 'key', key: 'Enter', windows_vk: 13, down: true },
      { kind: 'key', key: 'Enter', windows_vk: 13, down: false },
    ]);

    events.length = 0;
    input.dispatchEvent(
      new InputEvent('beforeinput', { inputType: 'insertText', data: 'a', bubbles: true }),
    );
    expect(events, 'typed text crosses whole').toMatchObject([{ kind: 'text', text: 'a' }]);

    events.length = 0;
    input.dispatchEvent(new CompositionEvent('compositionend', { data: 'é', bubbles: true }));
    expect(events, 'composed text crosses whole').toMatchObject([{ kind: 'text', text: 'é' }]);

    // **A keyboard path that inserts without `beforeinput` still types.**
    events.length = 0;
    input.value = 'b';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect(events, 'the value that landed is the text the page is owed').toMatchObject([
      { kind: 'text', text: 'b' },
    ]);
    expect(input.value, 'and the field is cleared for the next entry').toBe('');
  });

  /// **A horizontal drag scrolls, and never turns into a click.** Measuring
  /// only y left a horizontal swipe un-moved, so the lift clicked the page.
  it('sends a horizontal drag as a wheel, never a click', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(finger('pointerdown', 60, 100));
    canvas.dispatchEvent(finger('pointermove', 30, 100));
    canvas.dispatchEvent(finger('pointerup', 30, 100));

    const wheel = events.find((event) => event.kind === 'wheel');
    if (wheel?.kind !== 'wheel') throw new Error('the horizontal drag sent no wheel');
    expect(wheel.delta_x, 'a finger moving left scrolls right').toBeGreaterThan(0);
    expect(
      events.filter((event) => event.kind === 'mouse'),
      'and the lift is no click',
    ).toHaveLength(0);
  });

  /// **A cancelled touch is dropped, not clicked.** The system cancels for an
  /// edge-back, the notification shade or a backgrounding; the tap path
  /// activated whatever was under the finger.
  it('drops a cancelled touch and never clicks the page', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointercancel', 10, 20));
    expect(events, 'a cancelled touch sends nothing').toHaveLength(0);
  });

  /// A mouse cancellation still owes the page its release.
  it('releases the mouse on a cancelled mouse gesture', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(
      new MouseEvent('pointercancel', { clientX: 4, clientY: 6, bubbles: true }),
    );
    expect(events, 'the mouse release still crosses').toMatchObject([
      { kind: 'mouse', action: 'release' },
    ]);
  });

  /// **A swipe does not raise the keyboard, a tap does.** Focusing on every
  /// touch pointerdown opened the keyboard and its resize cancelled the
  /// swipe it interrupted.
  it('raises the keyboard on a tap, not on a swipe', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    const canvas = target.querySelector('canvas');
    if (input === null || canvas === null) throw new Error('the surface is missing its parts');
    input.blur();
    expect(document.activeElement, 'nothing is focused before a gesture').not.toBe(input);

    canvas.dispatchEvent(finger('pointerdown', 60, 100));
    canvas.dispatchEvent(finger('pointermove', 30, 100));
    canvas.dispatchEvent(finger('pointerup', 30, 100));
    expect(document.activeElement, 'a swipe does not focus the field').not.toBe(input);

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    expect(document.activeElement, 'a tap does').toBe(input);
  });

  /// **The WebView clears a focus its own tap did not set.** A canvas is not
  /// focusable, so Chromium takes the keyboard back to the body right after
  /// the gesture; the next frame puts it on the hidden field. Without that
  /// the phone's typing landed in the chat composer behind the picture
  /// (measured on the dev stack).
  it('takes the keyboard back after the webview clears it', async () => {
    vi.stubGlobal('requestAnimationFrame', (fn: () => void) => setTimeout(fn, 0));
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    const canvas = target.querySelector('canvas');
    if (input === null || canvas === null) throw new Error('the surface is missing its parts');

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    // The WebView's own handling of the tap, which runs after the handler.
    input.blur();
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(document.activeElement, 'the next frame gives the keyboard to the field').toBe(input);
  });

  /// **A composition re-fired as an input is sent once.** Some platforms
  /// commit the IME text and then fire the final input again with the value
  /// re-set; matching the committed text once keeps the page from receiving
  /// the same words twice.
  it('sends a composition once when the platform re-fires it as input', async () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('no keyboard field');

    input.dispatchEvent(new CompositionEvent('compositionend', { data: 'é', bubbles: true }));
    input.value = 'é';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect(events, 'the committed text crosses once').toMatchObject([{ kind: 'text', text: 'é' }]);
    expect(input.value, 'and the re-fired value is cleared').toBe('');

    // **A later direct insert of the same string is not the commit
    // re-firing**, so the latch is cleared on the next task and this sends.
    await new Promise((resolve) => setTimeout(resolve, 1));
    events.length = 0;
    input.value = 'é';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    expect(events, 'the later insert is sent').toMatchObject([{ kind: 'text', text: 'é' }]);
  });
});

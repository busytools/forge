// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * The phone's picture: no native overlay exists there, so the page paints
 * each relayed frame on a canvas and sends the person's gestures back. The
 * desktop's arm (the shell's overlay) is not this test's.
 */
vi.mock('./link', () => ({
  canAttach: vi.fn(() => true),
  clearBrowserLink: vi.fn(),
  drawsSurface: vi.fn(() => true),
  drawsCanvas: vi.fn(() => true),
  keptFrame: vi.fn(() => Promise.resolve(null)),
  showBrowser: vi.fn(() => Promise.resolve(null)),
  hideBrowser: vi.fn(() => Promise.resolve(true)),
  installBrowserLink: vi.fn(),
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
  navigateBrowser: vi.fn(() => Promise.resolve()),
  onConnection: vi.fn(() => Promise.resolve(() => {})),
  onFailed: vi.fn(() => Promise.resolve(() => {})),
  onLower: vi.fn(() => null),
  onNav: vi.fn(() => Promise.resolve(() => {})),
  onRemeasure: vi.fn(() => null),
  onTabs: vi.fn(() => Promise.resolve(() => {})),
  onView: vi.fn(() => Promise.resolve(() => {})),
  reattachesOnReconnect: vi.fn(() => false),
  reportSurfaceRect: vi.fn(() => Promise.resolve()),
  resizeBrowser: vi.fn(() => Promise.resolve()),
  sizesAsWatcher: vi.fn(() => false),
  takeControl: vi.fn(() => Promise.resolve()),
  usesPageHistory: vi.fn(() => false),
}));

/**
 * The perf marks a gesture and a painted frame leave (#2645), captured
 * instead of written: the probe's file sink is not this test's subject, and
 * the record's shape is `perf.test.ts`'s.
 */
const perf = vi.hoisted(() => ({ input: vi.fn(), paint: vi.fn() }));
vi.mock('../perf', () => ({
  noteBrowserInput: perf.input,
  noteBrowserPaint: perf.paint,
}));

import BrowserSurface from './BrowserSurface.svelte';
import { keptFrame } from './link';
import { selectTab, setRegion, surface } from './surface.svelte';
import type { BrowserFrame, Connection } from '../socket';
import type { BrowserInputEvent } from '../protocol';

/** The frames and gestures a stubbed connection carried. */
function stubConnection(takes = true) {
  const listeners = new Set<(frame: BrowserFrame) => void>();
  const events: BrowserInputEvent[] = [];
  const connection = {
    onBrowserFrame: (fn: (frame: BrowserFrame) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    browserInput: (event: BrowserInputEvent) => {
      if (!takes) return false;
      events.push(event);
      return true;
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
    page_width: 100,
    page_height: 50,
    bytes: new Uint8Array([1]),
  };
}

beforeEach(() => {
  surface.phase = 'closed';
  surface.id = null;
  surface.tab = null;
  surface.viewer = null;
  surface.controller = null;
  surface.attachedProfile = null;
  surface.profile = null;
  surface.editable = null;
  perf.input.mockClear();
  perf.paint.mockClear();
  vi.mocked(keptFrame).mockReset();
  vi.mocked(keptFrame).mockResolvedValue(null);
  // jsdom has no 2d context and prints a notice per paint; the frame's SIZE
  // is this test's subject, not the pixels.
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null);
});

afterEach(() => {
  // **A live surface's unmount closes itself asynchronously.** Left to do
  // that after this test, its reset lands in the next one and clears the id
  // the test just set; a synchronous close first keeps the teardown here.
  surface.phase = 'closed';
  if (shown !== null) void unmount(shown);
  shown = null;
  document.body.innerHTML = '';
  vi.unstubAllGlobals();
});

describe('the canvas viewer', () => {
  /// **A pick out of a failed surface paints.** The surface store now moves
  /// the phase to live on a successful attach; this is the page's half of
  /// that contract, where the failed state had no canvas at all.
  it('paints on a surface that recovered from failed by a pick', async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 320, height: 200, close: () => {} }),
    );
    vi.mocked(keptFrame).mockResolvedValue(frame(1));
    const { showBrowser } = await import('./link');
    vi.mocked(showBrowser).mockResolvedValue({ profile: 'hunt', tab: 2 });
    const { connection } = stubConnection();
    surface.phase = 'failed';
    surface.profile = 'hunt';
    surface.reason = 'the browser host exited';
    surface.seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    const target = document.createElement('div');
    document.body.appendChild(target);
    shown = mount(BrowserSurface, { target, props: { connection } });
    flushSync();
    expect(target.querySelector('canvas'), 'failed draws no canvas').toBeNull();

    // The shell's `browser_view` names the profile id the frame filter
    // reads; it rides the same attach this pick asks for.
    surface.id = 1;
    expect(await selectTab(2)).toBeNull();
    flushSync();
    expect(surface.phase, 'the pick recovered the surface').toBe('live');
    expect(surface.reason).toBeNull();
    const canvas = target.querySelector('canvas');
    expect(canvas, 'the live surface draws its canvas').not.toBeNull();
    await vi.waitFor(() => expect(canvas?.width, 'and the kept frame paints').toBe(100));
  });

  /// **A kept-frame ask that missed is asked again.** It can land before the
  /// canvas is bound, or before the attach's key frame reaches the shell; a
  /// single ask that returned null left a still page blank for good.
  it('asks the kept frame again when the first answer is nothing', async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 320, height: 200, close: () => {} }),
    );
    vi.mocked(keptFrame).mockResolvedValueOnce(null).mockResolvedValueOnce(frame(5));
    const { connection } = stubConnection();
    surface.id = 1;
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('the canvas is not drawn');

    await vi.waitFor(
      () => {
        expect(vi.mocked(keptFrame).mock.calls.length, 'the miss is retried').toBeGreaterThan(1);
        expect(canvas.width, 'and the retried frame is painted').toBe(100);
      },
      { timeout: 2000 },
    );
  });

  /// **A decode finishes after the frame it was for is stale.** Two frames
  /// decode concurrently; the older one resolving last must not paint over
  /// the newer picture.
  it('does not paint an older frame that decoded last', async () => {
    let releaseOlder: (bitmap: {
      width: number;
      height: number;
      close: () => void;
    }) => void = () => {};
    const older = new Promise<{ width: number; height: number; close: () => void }>((resolve) => {
      releaseOlder = resolve;
    });
    let calls = 0;
    vi.stubGlobal('createImageBitmap', () => {
      calls += 1;
      if (calls === 1) return older;
      return Promise.resolve({ width: 222, height: 200, close: () => {} });
    });
    const { connection, listeners } = stubConnection();
    surface.id = 1;
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('the canvas is not drawn');

    const first: BrowserFrame = {
      profile: 1,
      tab: 2,
      seq: 6,
      width: 111,
      height: 50,
      key: false,
      page_width: 111,
      page_height: 50,
      bytes: new Uint8Array([1]),
    };
    const second: BrowserFrame = { ...first, seq: 7, width: 222, page_width: 222 };
    for (const listener of listeners) listener(first);
    await vi.waitFor(() => expect(calls).toBe(1));
    for (const listener of listeners) listener(second);
    await vi.waitFor(() => expect(canvas.width, 'the newer frame paints').toBe(222));

    releaseOlder({ width: 111, height: 50, close: () => {} });
    await Promise.resolve();
    await Promise.resolve();
    expect(canvas.width, 'the older decode does not paint over the newer').toBe(222);
  });

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
      page_width: 160,
      page_height: 100,
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

  /// **A relayed frame that overtakes an older one does not paint over it.**
  /// The tab's own last sequence is what its canvas compares against, so a
  /// frame that lost the race cannot put a stale picture on screen.
  it('never paints an older frame over a newer one on the same tab', async () => {
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

    for (const listener of listeners) listener({ ...frame(5), width: 500, height: 250 });
    await vi.waitFor(() => expect(canvas.width).toBe(500));
    for (const listener of listeners) listener({ ...frame(4), width: 400, height: 200 });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(canvas.width, 'the older frame does not paint').toBe(500);
    expect(decoded, 'and is not even decoded').toHaveLength(1);
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
  it("maps the picture's point through the page the frame names, not the buffer's device pixels", async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 400, height: 200, close: () => {} }),
    );
    const { connection, listeners, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    flushSync();
    // A 400x200 frame of a 290x146 page (its own CSS viewport).
    for (const listener of listeners) {
      listener({ ...frame(0), width: 400, height: 200, page_width: 290, page_height: 146 });
    }
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
    setRegion({ x: 0, y: 0, width: 290, height: 146 }, { width: 390, height: 844 }, 1);

    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 10, clientY: 5, bubbles: true }));
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({ kind: 'mouse', action: 'press', x: 20, y: 10 });
  });

  /// **The page mapped through is the frame's, not this surface's box.** An
  /// agent's `browser_resize` under the controller lays the page out at
  /// another size; the picture is letterboxed here and a tap must land on the
  /// page point the picture shows. Mapping through the surface's own area
  /// sent it to a different point.
  it('maps a tap into the page the frame shows, not into the surface box', async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 400, height: 200, close: () => {} }),
    );
    const { connection, listeners, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    flushSync();
    // Painted at 2x from a page laid out 200x100 CSS.
    for (const listener of listeners) {
      listener({ ...frame(0), width: 400, height: 200, page_width: 200, page_height: 100 });
    }
    await vi.waitFor(() => expect(canvas.width).toBe(400));
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
    // A page area of a different size again: the mapping must not read it.
    setRegion({ x: 0, y: 0, width: 290, height: 146 }, { width: 390, height: 844 }, 1);

    canvas.dispatchEvent(
      new MouseEvent('pointerdown', { clientX: 72.5, clientY: 36.5, bubbles: true }),
    );
    expect(events).toHaveLength(1);
    expect(events[0], 'the centre of the picture is the centre of the page').toMatchObject({
      kind: 'mouse',
      action: 'press',
      x: 100,
      y: 50,
    });
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

  /**
   * **The round trip is what the perf log has to show** (#2645): a gesture
   * as it leaves, and the frame it draws. A pointer move is a stream, so it
   * is not a mark of its own.
   */
  it('marks a gesture and the frame it answered', async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 100, height: 50, close: () => {} }),
    );
    const { connection, listeners } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    surface.profile = 'Busytools/default';
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();

    canvas.dispatchEvent(new MouseEvent('pointermove', { clientX: 3, clientY: 4, bubbles: true }));
    expect(perf.input, 'a pointer move is a stream, not a step').not.toHaveBeenCalled();

    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 3, clientY: 4, bubbles: true }));
    expect(perf.input, 'the press is marked where it leaves').toHaveBeenCalledWith(
      'mouse:press',
      2,
      'Busytools/default',
    );

    for (const listener of listeners) listener(frame(4));
    await vi.waitFor(() => expect(canvas.width).toBe(100));
    expect(perf.paint, 'and the frame it drew, with its own sequence').toHaveBeenCalledWith(
      4,
      2,
      'Busytools/default',
    );
  });

  /// **A key is marked once, at the press**: the release is part of the same
  /// gesture and would pair the frame with the wrong end of the keystroke.
  it('marks a key at its press, not at its release', () => {
    const { connection } = stubConnection();
    const target = open(connection);
    surface.id = 1;
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();
    const ime = target.querySelector('.surface-ime');
    if (!(ime instanceof HTMLInputElement)) throw new Error('the page has no keyboard field');
    ime.dispatchEvent(
      new KeyboardEvent('keydown', {
        key: 'Enter',
        code: 'Enter',
        bubbles: true,
        cancelable: true,
      }),
    );
    expect(perf.input, 'one gesture, one mark').toHaveBeenCalledTimes(1);
    expect(perf.input).toHaveBeenCalledWith('key', 2, null);
  });

  /// **The rest of the gesture vocabulary is marked too**: a release, inserted
  /// text and a wheel each leave their own step.
  it('marks a release, inserted text and a wheel', () => {
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    const ime = target.querySelector('.surface-ime');
    if (canvas === null || !(ime instanceof HTMLInputElement)) {
      throw new Error('the page area or its keyboard field is missing');
    }
    surface.id = 1;
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();

    canvas.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: 40, bubbles: true, cancelable: true }));
    ime.value = 'x';
    ime.dispatchEvent(new InputEvent('input', { bubbles: true }));

    expect(perf.input, 'the release is a step of its own').toHaveBeenCalledWith(
      'mouse:release',
      2,
      null,
    );
    expect(perf.input, 'and the wheel').toHaveBeenCalledWith('wheel', 2, null);
    expect(perf.input, 'and inserted text').toHaveBeenCalledWith('text', 2, null);
    expect(events, 'one gesture each, all sent').toHaveLength(3);
  });

  /// **A gesture the socket refused never left**, so it leaves no mark: the
  /// log would pair it with a frame it never asked for.
  it('marks nothing for a gesture the socket refused', () => {
    const { connection, events } = stubConnection(false);
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();
    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    expect(events, 'the socket took nothing').toHaveLength(0);
    expect(perf.input, 'and the mark goes with it').not.toHaveBeenCalled();
  });

  /// **A mouse's press keeps the keyboard on the page.** The click moves
  /// focus to the body and the canvas is not focusable, so without this a
  /// page with no shell behind it loses every key after the first click.
  it('puts the keyboard on the page for a press that holds the role', async () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    const ime = target.querySelector('input.surface-ime');
    if (canvas === null || ime === null) throw new Error('no canvas or keyboard field');
    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    await vi.waitFor(() =>
      expect(document.activeElement, 'a click that holds the role owns the keyboard').toBe(ime),
    );
  });

  /// **A watcher's press takes no keyboard.** Its keys are refused by the
  /// server, and a field it does not own must not take the person's typing.
  it('leaves the keyboard alone for a press that does not hold the role', () => {
    surface.viewer = 7;
    surface.controller = null;
    const { connection } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    const ime = target.querySelector('input.surface-ime');
    if (canvas === null || ime === null) throw new Error('no canvas or keyboard field');
    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    expect(document.activeElement, "a watcher's press takes no keyboard").not.toBe(ime);
  });

  /// **A hover is the page's too**, and a double click is the count the DOM
  /// already decided: a menu that opens on hover and a field that selects on
  /// a double click both need what a drag-only, single-click stream loses.
  it('sends a button-less move and a double click as itself', () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(
      new MouseEvent('pointermove', { clientX: 10, clientY: 20, bubbles: true }),
    );
    expect(events.at(-1), 'a hover crosses with no button held').toMatchObject({
      kind: 'mouse',
      action: 'move',
    });

    canvas.dispatchEvent(
      new MouseEvent('pointerdown', { clientX: 10, clientY: 20, detail: 2, bubbles: true }),
    );
    expect(events.at(-1), 'the second click is the second click').toMatchObject({
      action: 'press',
      clicks: 2,
    });
  });

  /// **A mouse that takes control can type at once.** A fine pointer's
  /// keyboard is physical, so the field simply follows the role; a finger's
  /// keyboard follows the page's focus instead, and the tests above pin
  /// that.
  it('gives the keyboard to a fine pointer that takes control', async () => {
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: query === '(pointer: fine)' }));
    const { connection } = stubConnection();
    const target = open(connection);
    surface.viewer = 7;
    surface.controller = 7;
    flushSync();
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('no keyboard field');
    await vi.waitFor(() =>
      expect(document.activeElement, 'the field takes the keyboard').toBe(input),
    );
  });

  /// **A fine pointer's refocus must not follow the page's focus answer.**
  /// On the web and the desktop a click on a button or empty page reports
  /// `editable: false` - the normal state - and the queued refocus is what
  /// keeps the keys on the page after the browser clears them. Gating it on
  /// the page's answer left the field blurred and the page unable to type
  /// until an editable field was clicked.
  it('keeps a fine pointer on the page whatever the focus answer says', async () => {
    vi.stubGlobal('requestAnimationFrame', (fn: () => void) => setTimeout(fn, 0));
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: query === '(pointer: fine)' }));
    surface.editable = false;
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    const canvas = target.querySelector('canvas');
    if (input === null || canvas === null) throw new Error('no canvas or keyboard field');

    canvas.dispatchEvent(new MouseEvent('pointerdown', { clientX: 4, clientY: 6, bubbles: true }));
    // The WebView's own handling of the press, which runs after the handler.
    input.blur();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(document.activeElement, 'the refocus is the fine pointer one').toBe(input);
  });

  /// **A fine pointer's effect stops at the role.** Its branch returns
  /// before the page's answer, so `editable: false` - the normal report
  /// after a button click or an attach to a page with no field - must not
  /// blur the field the mouse just took. Deleting that return lets the
  /// coarse follow path run and the keys stop reaching the page.
  it("keeps a fine pointer's field when the answer turns non-editable", async () => {
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: query === '(pointer: fine)' }));
    const { connection } = stubConnection();
    const target = open(connection);
    surface.viewer = 7;
    surface.controller = 7;
    surface.editable = true;
    flushSync();
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('no keyboard field');
    await vi.waitFor(() =>
      expect(document.activeElement, 'the field takes the keyboard').toBe(input),
    );

    surface.editable = false;
    flushSync();
    expect(document.activeElement, 'a fine pointer follows the role, not the page').toBe(input);
  });

  /// **An unknown answer leaves the field alone.** `null` is a v32 server
  /// that cannot report focus; the effect must neither raise nor drop the
  /// keyboard on it, or a phone on that pairing loses what the tap path
  /// raised.
  it('leaves the field alone when the focus answer is unknown', async () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector<HTMLInputElement>('.surface-ime');
    if (ime === null) throw new Error('no keyboard field');

    surface.editable = true;
    flushSync();
    await vi.waitFor(() => expect(document.activeElement).toBe(ime));
    surface.editable = null;
    flushSync();
    expect(document.activeElement, 'an unknown answer neither raises nor drops').toBe(ime);
  });

  /** A pointer event of the kind a finger sends. */
  function finger(type: string, x: number, y: number): Event {
    const event = new MouseEvent(type, { clientX: x, clientY: y, bubbles: true });
    Object.defineProperty(event, 'pointerType', { value: 'touch' });
    Object.defineProperty(event, 'pointerId', { value: 3 });
    return event;
  }

  /** A phone's pointer is coarse: no mouse, so the keyboard follows the
   *  page's focus rather than the role. */
  function coarse(): void {
    vi.stubGlobal('matchMedia', () => ({ matches: false }));
  }

  /// **The page says when a text field has focus, and the keyboard follows
  /// it.** The canvas cannot tell a field from a button, so the browser host
  /// is the only source - and the field is what carries the phone's typing.
  it('raises the keyboard when the page reports an editable focus', async () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector('input.surface-ime');
    if (ime === null) throw new Error('no keyboard field');
    expect(document.activeElement, 'no focus is no keyboard').not.toBe(ime);

    surface.editable = true;
    flushSync();
    await vi.waitFor(() =>
      expect(document.activeElement, 'an editable node takes the keyboard').toBe(ime),
    );
  });

  /// **A tap is only a click.** On the phone this used to raise the keyboard
  /// whatever was under the finger; now the page's own focus decides, and a
  /// tap that focused no text field leaves the keyboard down.
  it('a tap on the page alone does not raise the keyboard', () => {
    coarse();
    // A v33 server's answer: no editable node holds focus.
    surface.editable = false;
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    const ime = target.querySelector('input.surface-ime');
    if (canvas === null || ime === null) throw new Error('no canvas or keyboard field');

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    expect(events, "the tap is still the page's click").toMatchObject([
      { kind: 'mouse', action: 'press' },
      { kind: 'mouse', action: 'release' },
    ]);
    expect(document.activeElement, 'nothing editable focused, no keyboard').not.toBe(ime);
  });

  /// **A v32 server has no focus answer, so the tap is still the way in.**
  /// The answer is carried as `null` there, and the old every-tap behaviour
  /// is what keeps a phone on that pairing able to type at all.
  it('raises the keyboard on a tap when the server sends no focus answer', () => {
    coarse();
    expect(surface.editable, 'nothing has answered yet').toBeNull();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector('input.surface-ime');
    const canvas = target.querySelector('canvas');
    if (ime === null || canvas === null) throw new Error('no canvas or keyboard field');

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    expect(document.activeElement, 'the old tap path still types').toBe(ime);
  });

  /// **A drop that lands inside the refocus frame wins.** `takeKeyboard`
  /// schedules one more focus a frame later; if the page answered
  /// non-editable in between, that frame must not raise the keyboard back.
  it('does not refocus after a drop that lands in the same frame', async () => {
    vi.stubGlobal('requestAnimationFrame', (fn: () => void) => setTimeout(fn, 0));
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector<HTMLInputElement>('.surface-ime');
    if (ime === null) throw new Error('no keyboard field');

    surface.editable = true;
    flushSync(); // the effect focuses, and schedules its refocus frame
    surface.editable = false;
    flushSync(); // the page's own answer blurs it
    expect(document.activeElement, 'the drop blurred the field').not.toBe(ime);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(document.activeElement, 'the queued frame does not raise it back').not.toBe(ime);
  });

  /// **A tap on the field that already holds focus is the way back to the
  /// keyboard.** The host reports focus *changes*, so the field a person
  /// typed in and then dismissed the keyboard over reports nothing when it
  /// is tapped again - and without this the phone could not type into it a
  /// second time.
  it('raises the keyboard again for a tap while a field already holds focus', async () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    const ime = target.querySelector<HTMLInputElement>('.surface-ime');
    if (canvas === null || ime === null) throw new Error('no canvas or keyboard field');

    surface.editable = true;
    flushSync();
    await vi.waitFor(() => expect(document.activeElement).toBe(ime));
    // **The follow path's refocus frame has landed.** Left queued, it fires
    // after the dismiss below and refocuses by itself, which would pass this
    // test with the tap path broken.
    await new Promise((resolve) => setTimeout(resolve, 50));

    // The phone's own dismiss: the keyboard is gone and the page still
    // holds the field, so no focus change is coming.
    ime.blur();
    expect(document.activeElement, 'the keyboard was dismissed').not.toBe(ime);

    canvas.dispatchEvent(finger('pointerdown', 10, 20));
    canvas.dispatchEvent(finger('pointerup', 10, 20));
    await vi.waitFor(() =>
      expect(document.activeElement, 'the same field takes the keyboard back').toBe(ime),
    );
  });

  /// **A non-editable focus drops the keyboard**: the person tapped away from
  /// the field, or the page cleared it.
  it('drops the keyboard when the page loses its editable focus', async () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector('input.surface-ime');
    if (ime === null) throw new Error('no keyboard field');
    surface.editable = true;
    flushSync();
    await vi.waitFor(() => expect(document.activeElement).toBe(ime));

    surface.editable = false;
    flushSync();
    expect(document.activeElement, 'focus left the field').not.toBe(ime);
  });

  /// **A watcher never keeps the phone's keyboard**, whatever the page says:
  /// its keys would be refused, and the field it does not own must not take
  /// the person's typing.
  it("drops the keyboard when the phone's viewer loses control", async () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const ime = target.querySelector('input.surface-ime');
    if (ime === null) throw new Error('no keyboard field');
    surface.editable = true;
    flushSync();
    await vi.waitFor(() => expect(document.activeElement).toBe(ime));

    surface.controller = null;
    flushSync();
    expect(document.activeElement, 'a watcher holds no keyboard').not.toBe(ime);
  });

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

  /// **A finger's displacement is the page's too.** A scaled picture - an
  /// agent's `browser_resize` under the controller, the case this surface's
  /// mapping exists for - must move the page by its own CSS pixels: 10 px
  /// over a half-size picture scrolls 20, not 10.
  it("fits a swipe's displacement to the page the picture shows", async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 400, height: 200, close: () => {} }),
    );
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, listeners, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    flushSync();
    // A 400x200 frame of a 290x146 page.
    for (const listener of listeners) {
      listener({ ...frame(0), width: 400, height: 200, page_width: 290, page_height: 146 });
    }
    await vi.waitFor(() => expect(canvas.width).toBe(400));
    // The picture is drawn at half the page's size.
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

    canvas.dispatchEvent(finger('pointerdown', 60, 60));
    canvas.dispatchEvent(finger('pointermove', 50, 50));
    canvas.dispatchEvent(finger('pointerup', 50, 50));

    const wheel = events.find((event) => event.kind === 'wheel');
    if (wheel?.kind !== 'wheel') throw new Error('the swipe sent no wheel');
    expect(wheel.delta_y, "the finger's 10 px is the page's 20").toBe(20);
    expect(wheel.delta_x, "and the width's 10 px is the page's 20").toBe(20);
  });

  /// **A wheel tick is an amount, not a displacement on the picture.** The
  /// page scrolls as far whatever size the picture is drawn at, so the same
  /// scaled picture must not fit the wheel's deltas the way it fits a finger's.
  it('sends the wheel as an amount, not fitted to the picture', async () => {
    vi.stubGlobal('createImageBitmap', () =>
      Promise.resolve({ width: 400, height: 200, close: () => {} }),
    );
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, listeners, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');
    surface.id = 1;
    flushSync();
    for (const listener of listeners) {
      listener({ ...frame(0), width: 400, height: 200, page_width: 290, page_height: 146 });
    }
    await vi.waitFor(() => expect(canvas.width).toBe(400));
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

    canvas.dispatchEvent(
      new WheelEvent('wheel', { deltaX: -30, deltaY: -40, bubbles: true, cancelable: true }),
    );

    const wheel = events.find((event) => event.kind === 'wheel');
    if (wheel?.kind !== 'wheel') throw new Error('the wheel sent nothing');
    expect(wheel.delta_x, "the wheel's own amount and sign, not the picture's").toBe(-30);
    expect(wheel.delta_y, "the wheel's own amount and sign, not the picture's").toBe(-40);
  });

  /// **A wheel crosses in the page's own sign, both ways.** The wire carries
  /// the page's sign and the server turns it back for CEF; negating it here
  /// as well put a web wheel the wrong way. A wheel down stays positive and
  /// a wheel up stays negative.
  it("sends a wheel in the page's own sign, both ways", () => {
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const canvas = target.querySelector('canvas');
    if (canvas === null) throw new Error('no canvas');

    canvas.dispatchEvent(
      new WheelEvent('wheel', { deltaX: 30, deltaY: 40, bubbles: true, cancelable: true }),
    );
    canvas.dispatchEvent(
      new WheelEvent('wheel', { deltaX: -30, deltaY: -40, bubbles: true, cancelable: true }),
    );

    const wheels = events.filter((event) => event.kind === 'wheel');
    const sent = (index: number) => {
      const wheel = wheels[index];
      if (wheel?.kind !== 'wheel') throw new Error(`wheel ${index} is missing`);
      return [wheel.delta_x, wheel.delta_y];
    };
    expect(sent(0), 'a wheel down and right stays positive').toEqual([30, 40]);
    expect(sent(1), 'a wheel up and left stays negative').toEqual([-30, -40]);
  });

  /// **The phone's keys and text reach the page.** The canvas has no fields,
  /// so a hidden one takes the keyboard while this surface holds control;
  /// named keys cross as key events and inserted text whole.
  it('sends keys and typed text from the hidden field', () => {
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection, events } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('no keyboard field');
    // **The page reported an editable focus**, which is what raises the
    // phone's keyboard and puts the field on it.
    surface.editable = true;
    flushSync();
    expect(document.activeElement, 'the field holds the keyboard').toBe(input);
    events.length = 0; // nothing has been typed yet

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

  /// **Neither a swipe nor a bare tap raises the keyboard.** A swipe never
  /// did - its keyboard resize would cancel the swipe it interrupted - and a
  /// tap no longer does either: the page's own focus is the only source, so
  /// a button, a link or empty page leaves the keyboard down.
  it('raises the keyboard on neither a swipe nor a bare tap', () => {
    coarse();
    // A v33 server's answer: no editable node holds focus.
    surface.editable = false;
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
    expect(document.activeElement, 'and neither does a tap on its own').not.toBe(input);
  });

  /// **The WebView clears a focus its own tap did not set.** A canvas is not
  /// focusable, so Chromium takes the keyboard back to the body right after
  /// the gesture; the next frame puts it on the hidden field. Without that
  /// the phone's typing landed in the chat composer behind the picture
  /// (measured on the dev stack).
  it('takes the keyboard back after the webview clears it', async () => {
    vi.stubGlobal('requestAnimationFrame', (fn: () => void) => setTimeout(fn, 0));
    coarse();
    surface.viewer = 7;
    surface.controller = 7;
    const { connection } = stubConnection();
    const target = open(connection);
    const input = target.querySelector<HTMLInputElement>('.surface-ime');
    if (input === null) throw new Error('the surface is missing its parts');

    // The page reported an editable focus; the follow path focused the field.
    surface.editable = true;
    flushSync();
    expect(document.activeElement, 'the field holds the keyboard').toBe(input);
    // The WebView's own handling, which runs after the follow path's frame.
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

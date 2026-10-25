// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./host', () => ({
  canHost: vi.fn(() => true),
  drawsSurface: vi.fn(() => true),
  drawsCanvas: vi.fn(() => false),
  keptFrame: vi.fn(() => Promise.resolve(null)),
  showBrowser: vi.fn(() => Promise.resolve({ profile: 'hunt', tab: 1 })),
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

import { reportSurfaceRect } from './host';
import BrowserSurface from './BrowserSurface.svelte';
import { surface } from './surface.svelte';
import type { Connection } from '../socket';

/**
 * The connection this surface draws through. The page area's measurement is
 * the whole subject here, so the two members the surface can reach are stubs.
 */
const connection = {
  onBrowserFrame: () => () => {},
  browserInput: () => {},
} as unknown as Connection;

let shown: ReturnType<typeof mount> | null = null;

function open(phase: 'opening' | 'live' | 'failed'): HTMLElement {
  surface.phase = phase;
  surface.profile = null;
  surface.reason = phase === 'failed' ? 'the browser host did not start' : null;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(BrowserSurface, { target, props: { connection } });
  flushSync();
  return target;
}

beforeEach(() => {
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  surface.epoch = 42;
  vi.mocked(reportSurfaceRect).mockClear();
});

afterEach(() => {
  if (shown !== null) void unmount(shown);
  shown = null;
  document.body.innerHTML = '';
});

describe('the browser surface', () => {
  it("reports the page area's box to the shell on mount", () => {
    open('live');
    expect(reportSurfaceRect).toHaveBeenCalledTimes(1);
    const [rect, viewport, scale, epoch] = vi.mocked(reportSurfaceRect).mock.calls[0] ?? [];
    expect(rect).toMatchObject({ x: 0, y: 0 });
    expect(viewport, "the page's own viewport rides with the area").toMatchObject({
      width: window.innerWidth,
      height: window.innerHeight,
    });
    expect(scale).toBe(window.devicePixelRatio || 1);
    expect(epoch, 'the report names the open it belongs to').toBe(42);
  });

  /// **A new open re-reports even when the layout has not moved.** The shell
  /// refuses a report measured for another open, so a retry or reopen at the
  /// same size still needs one report carrying the new epoch.
  it('reports again when a new open takes the surface', () => {
    open('live');
    const target = document.querySelector('.takeover-page');
    expect(target, 'the page area mounted').not.toBeNull();
    surface.epoch = 43;
    flushSync();
    expect(reportSurfaceRect, 'the new open is measured by its own report').toHaveBeenCalledTimes(
      2,
    );
    const [, , , epoch] = vi.mocked(reportSurfaceRect).mock.calls[1] ?? [];
    expect(epoch).toBe(43);
  });

  /// **The shell can ask for the measurement again.** A refused report was
  /// measured in a transient layout; no ResizeObserver fires when only the
  /// position moved, so the shell's remeasure is what places the surface.
  it('measures again when the shell asks for a remeasure', () => {
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
      frames.push(callback);
      return frames.length;
    });
    try {
      open('live');
      expect(reportSurfaceRect).toHaveBeenCalledTimes(1);
      window.dispatchEvent(new Event('forge-surface-remeasure'));
      expect(reportSurfaceRect, 'the remeasure waits for the next frame').toHaveBeenCalledTimes(1);
      expect(frames.length, 'and is queued on a frame').toBe(1);
      for (const callback of frames) callback(0);
      expect(reportSurfaceRect, 'the remeasure lands a fresh report').toHaveBeenCalledTimes(2);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('draws the opening state', () => {
    const target = open('opening');
    expect(target.textContent).toContain('opening the browser');
  });

  it("draws the shell's own sentence when the surface failed", () => {
    const target = open('failed');
    expect(target.textContent).toContain('the browser host did not start');
  });
});

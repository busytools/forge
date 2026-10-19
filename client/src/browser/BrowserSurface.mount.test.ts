// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./host', () => ({
  showBrowser: vi.fn(() => Promise.resolve(null)),
  hideBrowser: vi.fn(() => Promise.resolve(null)),
  reportSurfaceRect: vi.fn(() => Promise.resolve()),
}));

import { reportSurfaceRect } from './host';
import BrowserSurface from './BrowserSurface.svelte';
import { surface } from './surface.svelte';

let shown: ReturnType<typeof mount> | null = null;

function open(phase: 'opening' | 'live' | 'failed'): HTMLElement {
  surface.phase = phase;
  surface.profile = null;
  surface.reason = phase === 'failed' ? 'the browser host did not start' : null;
  const target = document.createElement('div');
  document.body.appendChild(target);
  shown = mount(BrowserSurface, { target });
  flushSync();
  return target;
}

beforeEach(() => {
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  vi.mocked(reportSurfaceRect).mockClear();
});

afterEach(() => {
  if (shown !== null) unmount(shown);
  shown = null;
  document.body.innerHTML = '';
});

describe('the browser surface', () => {
  it("reports the column's box to the shell on mount", () => {
    open('live');
    expect(reportSurfaceRect).toHaveBeenCalledTimes(1);
    const [rect, scale] = vi.mocked(reportSurfaceRect).mock.calls[0] ?? [];
    expect(rect).toMatchObject({ x: 0, y: 0 });
    expect(scale).toBe(window.devicePixelRatio || 1);
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

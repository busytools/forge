import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./host', () => ({
  canHost: vi.fn(() => true),
  drawsSurface: vi.fn(() => true),
  showBrowser: vi.fn(),
  hideBrowser: vi.fn(),
  reportSurfaceRect: vi.fn(),
}));

import { canHost, drawsSurface, hideBrowser, showBrowser } from './host';
import { closeSurface, openSurface, surface } from './surface.svelte';

const show = vi.mocked(showBrowser);
const hide = vi.mocked(hideBrowser);
const here = vi.mocked(drawsSurface);
const hostable = vi.mocked(canHost);

beforeEach(() => {
  show.mockReset();
  hide.mockReset();
  here.mockReset();
  here.mockReturnValue(true);
  hostable.mockReset();
  hostable.mockReturnValue(true);
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
});

describe('the browser surface', () => {
  it('opens onto the live phase and names the profile it opened', async () => {
    show.mockResolvedValue(null);
    expect(await openSurface('hunt')).toBeNull();
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('hunt');
    expect(show).toHaveBeenCalledWith('hunt', null);
  });

  it("carries the shell's own sentence when it could not open", async () => {
    show.mockResolvedValue('the profile is held by Busytools/forge/lead');
    expect(await openSurface(null)).toBe('the profile is held by Busytools/forge/lead');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the profile is held by Busytools/forge/lead');
  });

  it('closes without closing the browser: the shell is asked to hide only', async () => {
    show.mockResolvedValue(null);
    await openSurface('hunt');
    hide.mockResolvedValue({ lowered: true });
    expect(await closeSurface()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(surface.profile).toBeNull();
    expect(hide).toHaveBeenCalledWith('hunt');
  });

  it('keeps the takeover live over another profile the shell is still drawing', async () => {
    show.mockResolvedValue(null);
    await openSurface('hunt');
    hide.mockResolvedValue({ lowered: false });
    expect(await closeSurface()).toBeNull();
    expect(surface.phase, 'a lower of someone else leaves this page up').toBe('live');
    expect(surface.profile).toBe('hunt');
  });

  it('a slow open that lands after a close is taken down again', async () => {
    let resolveFirst: (why: string | null) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<string | null>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    hide.mockResolvedValue({ lowered: true });
    expect(await closeSurface()).toBeNull();
    resolveFirst(null);
    await first;
    expect(hide, 'the stale open is hidden, not left under a closed store').toHaveBeenCalledWith(
      'hunt',
    );
    expect(surface.phase).toBe('closed');
  });

  it('a phone draws its own takeover: the shell is asked without a page surface', async () => {
    here.mockReturnValue(false);
    show.mockResolvedValue(null);
    expect(await openSurface('hunt')).toBeNull();
    expect(surface.phase, 'the page draws nothing on the phone').toBe('closed');
    expect(show).toHaveBeenCalledWith('hunt', null);
  });

  it('outside the shell the failure belongs to the row, not a takeover', async () => {
    hostable.mockReturnValue(false);
    expect(await openSurface(null)).toBe('this page is not the client');
    expect(surface.phase).toBe('closed');
    expect(show).not.toHaveBeenCalled();
  });

  it('a slow open that was superseded never lands', async () => {
    let resolveFirst: (why: string | null) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<string | null>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    show.mockResolvedValueOnce(null);
    await openSurface('scout');
    resolveFirst(null);
    await first;
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('scout');
  });

  it("a refused hide leaves the surface's failure drawn rather than claiming it closed", async () => {
    show.mockResolvedValue(null);
    await openSurface(null);
    hide.mockResolvedValue('the browser host is gone');
    expect(await closeSurface()).toBe('the browser host is gone');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the browser host is gone');
  });
});

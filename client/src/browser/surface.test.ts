import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./host', () => ({
  showBrowser: vi.fn(),
  hideBrowser: vi.fn(),
  reportSurfaceRect: vi.fn(),
}));

import { hideBrowser, showBrowser } from './host';
import { closeSurface, openSurface, surface } from './surface.svelte';

const show = vi.mocked(showBrowser);
const hide = vi.mocked(hideBrowser);

beforeEach(() => {
  show.mockReset();
  hide.mockReset();
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
    hide.mockResolvedValue(null);
    expect(await closeSurface()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(surface.profile).toBeNull();
    expect(hide).toHaveBeenCalledWith('hunt');
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

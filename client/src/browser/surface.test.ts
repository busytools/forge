// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./host', () => ({
  canHost: vi.fn(() => true),
  drawsSurface: vi.fn(() => true),
  showBrowser: vi.fn(),
  hideBrowser: vi.fn(),
  reportSurfaceRect: vi.fn(),
  resizeBrowser: vi.fn(() => Promise.resolve()),
  takeControl: vi.fn(() => Promise.resolve()),
}));

import { canHost, drawsSurface, hideBrowser, resizeBrowser, showBrowser } from './host';
import { closeSurface, openSurface, setRegion, surface, takeControl } from './surface.svelte';

const show = vi.mocked(showBrowser);
const hide = vi.mocked(hideBrowser);
const here = vi.mocked(drawsSurface);
const hostable = vi.mocked(canHost);
const resize = vi.mocked(resizeBrowser);

beforeEach(() => {
  show.mockReset();
  hide.mockReset();
  here.mockReset();
  here.mockReturnValue(true);
  hostable.mockReset();
  hostable.mockReturnValue(true);
  resize.mockClear();
  surface.viewer = null;
  surface.controller = null;
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
});

describe('the browser surface', () => {
  /// **The page is laid out to the controller's surface, and only to it.**
  /// A watcher's window must not move the page under the person driving it.
  it('sizes the page to the controller surface, never a watcher', () => {
    surface.viewer = 7;
    surface.controller = 7;
    setRegion({ x: 0, y: 40, width: 800, height: 600 }, { width: 800, height: 640 }, 2, 1);
    expect(resize, 'device pixels, and the ratio they draw at').toHaveBeenCalledWith(1600, 1200, 2);
    resize.mockClear();
    surface.controller = 8;
    setRegion({ x: 0, y: 40, width: 800, height: 600 }, { width: 800, height: 640 }, 2, 1);
    expect(resize, "a watcher's window sends nothing").not.toHaveBeenCalled();
  });

  /// **Taking the role sends the size the surface already has**: the page
  /// resizes at the moment the person can drive it, not at the next layout.
  it('sends the surface size when the person takes control', async () => {
    surface.viewer = 7;
    surface.controller = null;
    setRegion({ x: 0, y: 40, width: 640, height: 480 }, { width: 640, height: 520 }, 1, 1);
    expect(resize, 'nothing goes out while watching').not.toHaveBeenCalled();
    await takeControl(true);
    expect(resize).toHaveBeenCalledWith(640, 480, 1);
  });

  it('opens onto the live phase and names the profile it opened', async () => {
    show.mockResolvedValue({ profile: 'hunt' });
    expect(await openSurface('hunt')).toBeNull();
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('hunt');
    expect(show).toHaveBeenCalledWith('hunt', null, surface.epoch);
  });

  /// **Every open is its own surface identity.** The rect a page reports is
  /// only allowed to place the open it was measured for, so each open mints a
  /// fresh epoch and the shell is told which one it is opening.
  it('mints an epoch per open and carries it to the shell', async () => {
    show.mockResolvedValue({ profile: 'hunt' });
    await openSurface('hunt');
    const first = surface.epoch;
    hide.mockResolvedValue({ lowered: true });
    await closeSurface();
    await openSurface('hunt');
    expect(surface.epoch, 'a second open is not the first').not.toBe(first);
    expect(show).toHaveBeenLastCalledWith('hunt', null, surface.epoch);
  });

  it("carries the shell's own sentence when it could not open", async () => {
    show.mockResolvedValue('the profile is held by Busytools/forge/lead');
    expect(await openSurface(null)).toBe('the profile is held by Busytools/forge/lead');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the profile is held by Busytools/forge/lead');
  });

  it('closes without closing the browser: the shell is asked to hide only', async () => {
    show.mockResolvedValue({ profile: 'hunt' });
    await openSurface('hunt');
    hide.mockResolvedValue({ lowered: true });
    expect(await closeSurface()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(surface.profile).toBeNull();
    expect(hide).toHaveBeenCalledWith('hunt', null);
  });

  it('keeps the takeover live over another profile the shell is still drawing', async () => {
    show.mockResolvedValue({ profile: 'hunt' });
    await openSurface('hunt');
    hide.mockResolvedValue({ lowered: false });
    expect(await closeSurface()).toBeNull();
    expect(surface.phase, 'a lower of someone else leaves this page up').toBe('live');
    expect(surface.profile).toBe('hunt');
  });

  it('a slow open that lands after a close is taken down again', async () => {
    let resolveFirst: (answer: { profile: string }) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<{ profile: string }>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    hide.mockResolvedValue({ lowered: true });
    expect(await closeSurface()).toBeNull();
    resolveFirst({ profile: 'hunt' });
    await first;
    expect(hide, 'the stale open is hidden, not left under a closed store').toHaveBeenCalledWith(
      'hunt',
    );
    expect(surface.phase).toBe('closed');
  });

  it('a phone draws its own takeover: the shell is asked without a page surface', async () => {
    here.mockReturnValue(false);
    show.mockResolvedValue({ profile: 'hunt' });
    expect(await openSurface('hunt')).toBeNull();
    expect(surface.phase, 'the page draws nothing on the phone').toBe('closed');
    expect(show).toHaveBeenCalledWith('hunt', null, surface.epoch);
  });

  it('outside the shell the failure belongs to the row, not a takeover', async () => {
    hostable.mockReturnValue(false);
    expect(await openSurface(null)).toBe('this page is not the client');
    expect(surface.phase).toBe('closed');
    expect(show).not.toHaveBeenCalled();
  });

  it('a slow open that was superseded never lands', async () => {
    let resolveFirst: (answer: { profile: string }) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<{ profile: string }>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    show.mockResolvedValueOnce({ profile: 'scout' });
    await openSurface('scout');
    resolveFirst({ profile: 'hunt' });
    await first;
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('scout');
  });

  /// **Every lower closes the store, whichever door asked for it.** The
  /// hardware Back dispatches the same word the dock and the strip do, and
  /// the listener is the page's own - a takeover opened from the strip has
  /// no dock mounted to fold it, and used to answer Back with nothing.
  it('a lowered word closes the surface at the page level', async () => {
    surface.phase = 'live';
    surface.profile = 'shared';
    surface.reason = 'why';
    hide.mockResolvedValue({ lowered: true });
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'lowered' }));
    await vi.waitFor(() => expect(surface.phase).toBe('closed'));
    expect(hide).toHaveBeenCalledWith('shared', null);
    expect(surface.profile).toBe(null);
    expect(surface.reason).toBe(null);
  });

  /// **A raise is not a lower.** The strip dispatches both words; only the
  /// lower closes the surface.
  it('a raised word leaves a live surface up', () => {
    surface.phase = 'live';
    surface.profile = 'shared';
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('shared');
  });

  it('a hand-off with no profile lowers the profile the shell actually raised', async () => {
    show.mockResolvedValue({ profile: 'busyblocks-lead' });
    const seat = { org: 'Busytools', project: 'busyblocks', label: 'lead' };
    expect(await openSurface(null, seat)).toBeNull();
    expect(surface.profile, "the shell's answer is the raised profile").toBe('busyblocks-lead');
    hide.mockResolvedValue({ lowered: true });
    await closeSurface();
    expect(hide, 'Done lowers that browser, not the shared one').toHaveBeenCalledWith(
      'busyblocks-lead',
      null,
    );
  });

  it("a refused hide leaves the surface's failure drawn rather than claiming it closed", async () => {
    show.mockResolvedValue({ profile: '@shared' });
    await openSurface(null);
    hide.mockResolvedValue('the browser host is gone');
    expect(await closeSurface()).toBe('the browser host is gone');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the browser host is gone');
  });
});

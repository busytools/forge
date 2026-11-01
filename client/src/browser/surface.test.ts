// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('./link', () => ({
  canAttach: vi.fn(() => true),
  clearBrowserLink: vi.fn(),
  drawsCanvas: vi.fn(() => false),
  drawsSurface: vi.fn(() => true),
  hideBrowser: vi.fn(),
  installBrowserLink: vi.fn(),
  keptFrame: vi.fn(() => Promise.resolve(null)),
  listTabs: vi.fn(),
  navigateBrowser: vi.fn(() => Promise.resolve()),
  onConnection: vi.fn(() => Promise.resolve(() => {})),
  onFailed: vi.fn(() => Promise.resolve(() => {})),
  onKeyboard: vi.fn(() => null),
  onLower: vi.fn(() => null),
  onNav: vi.fn(() => Promise.resolve(() => {})),
  onRemeasure: vi.fn(() => null),
  onTabs: vi.fn(() => Promise.resolve(() => {})),
  onView: vi.fn(() => Promise.resolve(() => {})),
  reattachesOnReconnect: vi.fn(() => false),
  reportSurfaceRect: vi.fn(),
  resizeBrowser: vi.fn(() => Promise.resolve()),
  showBrowser: vi.fn(),
  sizesAsWatcher: vi.fn(() => false),
  takeControl: vi.fn(() => Promise.resolve()),
  usesPageHistory: vi.fn(() => false),
}));

import type { BrowserTabList } from '../protocol';
import {
  canAttach,
  drawsSurface,
  hideBrowser,
  listTabs,
  onConnection,
  onFailed,
  onView,
  reattachesOnReconnect,
  resizeBrowser,
  showBrowser,
  sizesAsWatcher,
  takeControl as sendControl,
} from './link';
import {
  closeSurface,
  installSurfaceEvents,
  openSurface,
  selectTab,
  setRegion,
  surface,
  takeControl,
  uninstallBrowser,
} from './surface.svelte';

const show = vi.mocked(showBrowser);
const hide = vi.mocked(hideBrowser);
const tabs = vi.mocked(listTabs);
const here = vi.mocked(drawsSurface);
const hostable = vi.mocked(canAttach);
const resize = vi.mocked(resizeBrowser);

beforeEach(() => {
  show.mockReset();
  hide.mockReset();
  tabs.mockReset();
  tabs.mockResolvedValue({
    profile: 'hunt',
    id: 1,
    up: true,
    connected: true,
    closing_at_ms: null,
    rows: [],
  });
  here.mockReset();
  here.mockReturnValue(true);
  hostable.mockReset();
  hostable.mockReturnValue(true);
  resize.mockClear();
  vi.mocked(sendControl).mockClear();
  vi.mocked(sizesAsWatcher).mockReturnValue(false);
  surface.viewer = null;
  surface.controller = null;
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  surface.id = null;
  surface.tab = null;
  surface.seat = null;
  surface.tabs = null;
  surface.attachedProfile = null;
  surface.nav = { can_back: false, can_forward: false };
});

/** The link's own push handlers, captured as the surface installs its steps. */
interface Pushes {
  failed: (payload: unknown) => void;
  view: (state: Record<string, unknown>) => void;
}

/**
 * Install the surface's listeners over the mocked link, handing back the two
 * the server's failures and its `browser_view` push arrive on. A suite is
 * installed once per link, so the previous one is released first.
 */
async function pushes(): Promise<Pushes> {
  const seen: Pushes = { failed: () => {}, view: () => {} };
  vi.mocked(onFailed).mockImplementation((fn) => {
    seen.failed = fn;
    return Promise.resolve(() => {});
  });
  vi.mocked(onView).mockImplementation((fn) => {
    seen.view = fn;
    return Promise.resolve(() => {});
  });
  uninstallBrowser();
  await installSurfaceEvents();
  return seen;
}

describe('the browser surface', () => {
  /// **A page's attach died with its socket**, so a page reconnecting asks
  /// for it again rather than keeping a picture no gesture can reach. First
  /// in this file so its listener is the install's own registration.
  it('re-attaches the surface when the connection comes back', async () => {
    const seen: { notice: (() => void) | null } = { notice: null };
    vi.mocked(onConnection).mockImplementation((fn) => {
      seen.notice = fn;
      return Promise.resolve(() => {});
    });
    vi.mocked(reattachesOnReconnect).mockReturnValue(true);
    const { installSurfaceEvents } = await import('./surface.svelte');
    await installSurfaceEvents();
    if (seen.notice === null) throw new Error('the page follows the connection');
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValue({ profile: 'hunt', tab: 2 });
    await openSurface('hunt', seat, 2);
    show.mockClear();
    seen.notice();
    await vi.waitFor(() => expect(show).toHaveBeenCalledWith('hunt', seat, 2));
  });

  /// **The page is laid out to the controller's surface, and only to it.**
  /// A watcher's window must not move the page under the person driving it.
  it('sizes the page to the controller surface, never a watcher', () => {
    surface.viewer = 7;
    surface.controller = 7;
    setRegion({ x: 0, y: 40, width: 800, height: 600 }, { width: 800, height: 640 }, 2);
    expect(resize, 'device pixels, and the ratio they draw at').toHaveBeenCalledWith(1600, 1200, 2);
    resize.mockClear();
    surface.controller = 8;
    setRegion({ x: 0, y: 40, width: 800, height: 600 }, { width: 800, height: 640 }, 2);
    expect(resize, "a watcher's window sends nothing").not.toHaveBeenCalled();
  });

  /// **A page with no shell sizes its own view while watching.** No other
  /// client is behind it, so the layout must not wait for the role.
  it('sizes the page while watching where the link says the page is alone', () => {
    vi.mocked(sizesAsWatcher).mockReturnValue(true);
    surface.viewer = 7;
    surface.controller = 8;
    setRegion({ x: 0, y: 40, width: 800, height: 600 }, { width: 800, height: 640 }, 2);
    expect(resize, 'the page lays itself out without taking the role').toHaveBeenCalledWith(
      1600,
      1200,
      2,
    );
  });

  /// **Taking the role sends the size the surface already has**: the page
  /// resizes at the moment the person can drive it, not at the next layout.
  it('sends the surface size when the person takes control', async () => {
    surface.viewer = 7;
    surface.controller = null;
    setRegion({ x: 0, y: 40, width: 640, height: 480 }, { width: 640, height: 520 }, 1);
    expect(resize, 'nothing goes out while watching').not.toHaveBeenCalled();
    await takeControl(true);
    expect(resize).toHaveBeenCalledWith(640, 480, 1);
  });

  /// **The press is not the role.** A take the server refuses leaves this
  /// surface a watcher, or the bar reads "release control" over a page whose
  /// every gesture the server then refuses; only the server's push settles it.
  it('does not claim the role before the server has given it', async () => {
    surface.viewer = 7;
    surface.controller = null;
    await takeControl(true);
    expect(sendControl, 'the take is asked for on the wire').toHaveBeenCalledWith(true);
    expect(surface.controller, 'a refused take is not a role').toBeNull();
  });

  it('opens onto the live phase and names the profile and tab it opened', async () => {
    show.mockResolvedValue({ profile: 'hunt', tab: 2 });
    expect(await openSurface('hunt', null, 2)).toBeNull();
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('hunt');
    expect(surface.tab).toBe(2);
    expect(show).toHaveBeenCalledWith('hunt', null, 2);
  });

  /// **A picker's switch is a re-attach on the same seat**, and the tab the
  /// shell answers with is the one the surface filters frames by.
  it('moves the surface to another tab through the same seat', async () => {
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 3 });
    expect(await selectTab(3)).toBeNull();
    expect(show).toHaveBeenLastCalledWith('hunt', seat, 3);
    expect(surface.tab).toBe(3);
  });

  /// **A pick out of a failed surface makes it live again.** Attaching used
  /// to leave the phase and the reason behind, so the bar called a working
  /// page failed and the phone never painted.
  it('recovers a failed surface when a pick attaches', async () => {
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce('the browser host exited');
    await openSurface('hunt', seat, 2);
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the browser host exited');
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 4 });
    expect(await selectTab(4)).toBeNull();
    expect(surface.phase).toBe('live');
    expect(surface.reason).toBeNull();
    expect(surface.tab).toBe(4);
  });

  /// **A pick that failed leaves no role behind**: the attach died with its
  /// connection, so the bar must draw neither a viewer, a controller, nor
  /// arrows for a page that is not there.
  it('clears the dead connection role and arrows when a pick fails', async () => {
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);
    surface.viewer = 7;
    surface.controller = 7;
    surface.nav = { can_back: true, can_forward: true };
    show.mockResolvedValueOnce('the browser host exited');
    expect(await selectTab(2)).toBe('the browser host exited');
    expect(surface.viewer, 'no viewer survives a failed pick').toBeNull();
    expect(surface.controller, 'no controller survives a failed pick').toBeNull();
    expect(surface.nav, 'no arrows survive a failed pick').toEqual({
      can_back: false,
      can_forward: false,
    });
  });

  /// **A pick superseded by a newer open is not the surface's to change.**
  /// Neither its refusal nor its success may revive or move what the open
  /// put up.
  it('drops a pick that a newer open superseded', async () => {
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);

    let release: (answer: string | { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<string | { profile: string; tab: number }>((resolve) => {
        release = resolve;
      }),
    );
    const picking = selectTab(3);

    show.mockResolvedValueOnce({ profile: 'hunt', tab: 5 });
    await openSurface('hunt', seat, 5);

    release('the browser host exited');
    expect(await picking).toBe('the browser host exited');
    expect(surface.phase, 'a stale refusal does not fail the newer surface').toBe('live');
    expect(surface.reason).toBeNull();
    expect(surface.tab).toBe(5);
  });

  it('drops a superseded pick answer that would move the newer surface', async () => {
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);

    let release: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        release = resolve;
      }),
    );
    const picking = selectTab(3);

    show.mockResolvedValueOnce({ profile: 'hunt', tab: 5 });
    await openSurface('hunt', seat, 5);

    release({ profile: 'hunt', tab: 4 });
    expect(await picking).toBeNull();
    expect(surface.tab, 'a stale success does not move the newer surface').toBe(5);
    expect(surface.phase).toBe('live');
  });

  /// **A slow tab read does not overwrite a newer one's rows.**
  it('keeps the newest tab list when an older read lands last', async () => {
    let releaseOld: (list: BrowserTabList) => void = () => {};
    tabs.mockReturnValueOnce(
      new Promise<BrowserTabList>((resolve) => {
        releaseOld = resolve;
      }),
    );
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', null, 1);

    const fresh: BrowserTabList = {
      profile: 'hunt',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [],
    };
    tabs.mockResolvedValueOnce(fresh);
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 2 });
    await openSurface('hunt', null, 2);
    await Promise.resolve();
    await Promise.resolve();
    expect(surface.tabs).toEqual(fresh);

    releaseOld({ ...fresh, id: 99 });
    await Promise.resolve();
    await Promise.resolve();
    expect(surface.tabs, 'the older read is refused').toEqual(fresh);
  });

  /// **A list for another profile never becomes the picker's rows**: a pick
  /// from it would ask for another browser's tab number.
  it('refuses a tab list for another profile', async () => {
    const mine: BrowserTabList = {
      profile: 'hunt',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [],
    };
    tabs.mockResolvedValue(mine);
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', null, 1);
    surface.attachedProfile = 'hunt';
    await Promise.resolve();
    await Promise.resolve();

    tabs.mockResolvedValueOnce({ ...mine, profile: 'other' });
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 2 });
    await selectTab(2);
    await Promise.resolve();
    await Promise.resolve();
    expect(surface.tabs, 'the other profile list is refused').toEqual(mine);
  });

  it("carries the shell's own sentence when it could not open", async () => {
    show.mockResolvedValue('the profile is held by Busytools/forge/lead');
    expect(await openSurface(null)).toBe('the profile is held by Busytools/forge/lead');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the profile is held by Busytools/forge/lead');
  });

  it('closes without closing the browser: the shell is asked to hide only', async () => {
    show.mockResolvedValue({ profile: 'hunt', tab: 1 });
    await openSurface('hunt');
    hide.mockResolvedValue(true);
    expect(await closeSurface()).toBeNull();
    expect(surface.phase).toBe('closed');
    expect(surface.profile).toBeNull();
    expect(hide).toHaveBeenCalledWith('hunt', null);
  });

  it('a slow open that lands after a close is taken down again', async () => {
    let resolveFirst: (answer: { profile: string; tab: number }) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    hide.mockResolvedValue(true);
    expect(await closeSurface()).toBeNull();
    resolveFirst({ profile: 'hunt', tab: 1 });
    await first;
    expect(hide, 'the stale open is hidden, not left under a closed store').toHaveBeenCalledWith(
      'hunt',
    );
    expect(surface.phase).toBe('closed');
  });

  it('a phone draws its own takeover: the shell is asked without a page surface', async () => {
    here.mockReturnValue(false);
    show.mockResolvedValue({ profile: 'hunt', tab: 1 });
    expect(await openSurface('hunt')).toBeNull();
    expect(surface.phase, 'the page draws nothing on the phone').toBe('closed');
    expect(show).toHaveBeenCalledWith('hunt', null, null);
  });

  it('outside the shell the failure belongs to the row, not a takeover', async () => {
    hostable.mockReturnValue(false);
    expect(await openSurface(null)).toBe('this page is not the client');
    expect(surface.phase).toBe('closed');
    expect(show).not.toHaveBeenCalled();
  });

  it('a slow open that was superseded never lands', async () => {
    let resolveFirst: (answer: { profile: string; tab: number }) => void = () => undefined;
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        resolveFirst = resolve;
      }),
    );
    const first = openSurface('hunt');
    show.mockResolvedValueOnce({ profile: 'scout', tab: 1 });
    await openSurface('scout');
    resolveFirst({ profile: 'hunt', tab: 1 });
    await first;
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('scout');
  });

  /**
   * **A failure can land between the shell's attach answer and the page's
   * open promise** (#2575). It names the pair that attached, so it must fail
   * the page the open lands on rather than being dropped and leaving a live
   * store over a page the shell has already lowered.
   */
  it('fails the open whose pair failed while the page was opening', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const opening = openSurface('hunt', seat, 2);
    push.view({ id: 3, tab: 2, profile: 'hunt', viewer: 7, controller: null, viewers: 1 });
    push.failed({ id: 3, tab: 2, reason: 'the browser host exited' });
    land({ profile: 'hunt', tab: 2 });

    expect(await opening, 'the open answers the failure that beat it').toBe(
      'the browser host exited',
    );
    expect(surface.phase, 'the page did not come up live over a dead page').toBe('failed');
    expect(surface.reason).toBe('the browser host exited');
  });

  /**
   * **A pair-carrying failure can beat the push that names the pair.** It is
   * remembered while the open is in flight and applied when the view push
   * proves it was this page's.
   */
  it('applies a remembered failure when the view push names its pair', async () => {
    const push = await pushes();
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const opening = openSurface('hunt', null, 2);
    push.failed({ id: 3, tab: 2, reason: 'the page stopped answering' });
    land({ profile: 'hunt', tab: 2 });

    expect(await opening, 'the answer alone cannot tell whose failure it was').toBeNull();
    push.view({ id: 3, tab: 2, profile: 'hunt' });
    expect(surface.phase, "the pair proves the failure was this page's").toBe('failed');
    expect(surface.reason).toBe('the page stopped answering');
  });

  it('refuses a failure for another pair while the page opens', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const opening = openSurface('hunt', seat, 2);
    push.view({ id: 3, tab: 2, profile: 'hunt' });
    // **The same tab number, another profile's id**: a tab-only match would
    // fail this page on a superseded attach's frame.
    push.failed({ id: 9, tab: 2, reason: 'another page stopped' });
    land({ profile: 'hunt', tab: 2 });

    expect(await opening).toBeNull();
    expect(surface.phase, "another page's failure is not this open's").toBe('live');
    expect(surface.reason).toBeNull();
  });

  /// **A nameless failure during an open belongs to the attach that sent it**,
  /// which may be one this open superseded: the open's own answer is what
  /// settles whether this page came up.
  it('drops a nameless failure while the page is opening', async () => {
    const push = await pushes();
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const opening = openSurface('hunt', null, 2);
    push.failed('the shell could not re-attach the viewer');
    // A failure applied here would lower the shell's page before the open
    // ever landed, which is the live-store-over-no-page shape.
    expect(hide, 'a superseded attach does not lower the newer page').not.toHaveBeenCalled();
    land({ profile: 'hunt', tab: 2 });

    expect(await opening).toBeNull();
    expect(surface.phase, 'a superseded attach does not fail the newer page').toBe('live');
  });

  /// **The control: a live surface still takes a failure that names no page.**
  it('fails a live surface on a failure that names no page', async () => {
    const push = await pushes();
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 2 });
    await openSurface('hunt', null, 2);
    push.failed({ reason: 'the browser is gone' });
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the browser is gone');
    expect(
      hide,
      'and the shell is told to lower the page it can no longer draw',
    ).toHaveBeenCalled();
  });

  /// **A live surface refuses a failure that names another pair**: a late
  /// frame for a superseded attach must not fail the tab now up.
  it('refuses a failure for another pair on a live surface', async () => {
    const push = await pushes();
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 2 });
    await openSurface('hunt', null, 2);
    push.view({ id: 3, tab: 2, profile: 'hunt' });
    push.failed({ id: 9, tab: 2, reason: 'another page stopped' });
    expect(surface.phase, "another pair's failure is not this page's").toBe('live');
    expect(surface.reason).toBeNull();
    expect(hide).not.toHaveBeenCalled();
  });

  /**
   * **A held failure lowers the page only once its pair is proved**
   * (#2575): while the open can still land, nothing is taken down; the view
   * push that names the pair is what settles it, and the answer returns the
   * same sentence rather than a silent success.
   */
  it('holds a failure during the open and answers with it at the landing', async () => {
    const push = await pushes();
    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const opening = openSurface('hunt', null, 2);
    push.failed({ id: 3, tab: 2, reason: 'the page stopped answering' });
    expect(hide, 'nothing is lowered while the open can still land').not.toHaveBeenCalled();
    expect(surface.phase, 'and the store has not failed yet').toBe('opening');

    push.view({ id: 3, tab: 2, profile: 'hunt' });
    expect(surface.phase, 'the pair proves it was this page').toBe('failed');
    expect(hide, 'so the shell lowers it once').toHaveBeenCalledTimes(1);

    land({ profile: 'hunt', tab: 2 });
    expect(await opening, 'and the open answers with the failure it found').toBe(
      'the page stopped answering',
    );
    expect(hide, 'without lowering a second time').toHaveBeenCalledTimes(1);
  });

  /// **A pick is a surface move too**: a failure for the tab being picked is
  /// held through the pick and applied when its landing names the pair.
  it('fails the pick whose new tab stopped while it was in flight', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);
    push.view({ id: 3, tab: 1, profile: 'hunt' });

    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const picking = selectTab(2);
    push.view({ id: 3, tab: 2, profile: 'hunt' });
    push.failed({ id: 3, tab: 2, reason: 'the picked page died' });
    land({ profile: 'hunt', tab: 2 });

    expect(await picking, 'the pick answers with the failure it found').toBe(
      'the picked page died',
    );
    expect(surface.phase, 'and the store does not go live over it').toBe('failed');
    expect(surface.reason).toBe('the picked page died');
  });

  /**
   * **A superseded pick does not close the window the newer one is using**
   * (#2726): `picking` is one flag, so an older pick's landing clears it only
   * while that pick is the current move. Otherwise a failure for the newer
   * tab is refused as another pair's and its page goes live over a server
   * that has already dropped it.
   */
  it('holds a failure for the newer pick when an older one lands first', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);
    push.view({ id: 3, tab: 1, profile: 'hunt' });

    let landOlder: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        landOlder = resolve;
      }),
    );
    const older = selectTab(2);

    let landNewer: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        landNewer = resolve;
      }),
    );
    const newer = selectTab(3);

    landOlder({ profile: 'hunt', tab: 2 });
    expect(await older, 'the older pick is superseded').toBeNull();

    // The view names the pair, then the failure arrives while the pick is
    // still in flight: held, and settled by the pick's own landing.
    push.view({ id: 3, tab: 3, profile: 'hunt' });
    push.failed({ id: 3, tab: 3, reason: 'the picked page died' });
    // The role this dead connection held, set after the push so nothing but
    // the pick's own failure path can clear it.
    surface.viewer = 7;
    surface.controller = 7;
    landNewer({ profile: 'hunt', tab: 3 });

    expect(await newer, 'the newer pick answers with the failure it found').toBe(
      'the picked page died',
    );
    expect(surface.phase, 'and does not go live over the dead page').toBe('failed');
    expect(surface.viewer, 'the dead connection role goes').toBeNull();
    expect(surface.controller, 'and no viewer is left holding it').toBeNull();
  });

  /// **A nameless failure during a pick is the attach's**, which the pick may
  /// have superseded: its answer settles whether the page came up.
  it('drops a nameless failure while a pick is in flight', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };
    show.mockResolvedValueOnce({ profile: 'hunt', tab: 1 });
    await openSurface('hunt', seat, 1);
    push.view({ id: 3, tab: 1, profile: 'hunt' });

    let land: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        land = resolve;
      }),
    );
    const picking = selectTab(2);
    push.failed('the shell could not re-attach the viewer');
    expect(hide, 'a superseded attach does not lower the page').not.toHaveBeenCalled();
    land({ profile: 'hunt', tab: 2 });

    expect(await picking).toBeNull();
    expect(surface.phase, 'the pick lands live').toBe('live');
  });

  /// **Each move starts clean**: a pick out of a failed surface must not
  /// inherit the sentence a previous move's push settled with.
  it('a pick out of a failed surface starts clean of the settled sentence', async () => {
    const push = await pushes();
    const seat = { org: 'Busytools', project: 'forge', label: 'lead' };

    let landOpen: (answer: { profile: string; tab: number }) => void = () => {};
    show.mockReturnValueOnce(
      new Promise<{ profile: string; tab: number }>((resolve) => {
        landOpen = resolve;
      }),
    );
    const opening = openSurface('hunt', seat, 2);
    // The failure is settled by the view push while the open is in flight.
    push.failed({ id: 3, tab: 2, reason: 'the page stopped answering' });
    push.view({ id: 3, tab: 2, profile: 'hunt' });
    expect(surface.phase, 'precondition: the push failed the page').toBe('failed');

    show.mockResolvedValueOnce({ profile: 'hunt', tab: 4 });
    expect(await selectTab(4), 'the pick reports only its own landing').toBeNull();
    expect(surface.phase, 'and brings the surface back live').toBe('live');

    landOpen({ profile: 'hunt', tab: 2 });
    await opening;
  });

  /// **Every lower closes the store, whichever door asked for it.** The
  /// hardware Back dispatches the same word the dock and the strip do, and
  /// the listener is the page's own - a takeover opened from the strip has
  /// no dock mounted to fold it, and used to answer Back with nothing.
  it('a lowered word closes the surface at the page level', async () => {
    surface.phase = 'live';
    surface.profile = 'hunt';
    surface.reason = 'why';
    hide.mockResolvedValue(true);
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'lowered' }));
    await vi.waitFor(() => expect(surface.phase).toBe('closed'));
    expect(hide).toHaveBeenCalledWith('hunt', null);
    expect(surface.profile).toBe(null);
    expect(surface.reason).toBe(null);
  });

  /// **A raise is not a lower.** The strip dispatches both words; only the
  /// lower closes the surface.
  it('a raised word leaves a live surface up', () => {
    surface.phase = 'live';
    surface.profile = 'hunt';
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'raised' }));
    expect(surface.phase).toBe('live');
    expect(surface.profile).toBe('hunt');
  });

  it('a hand-off with no profile lowers the profile the shell actually raised', async () => {
    show.mockResolvedValue({ profile: 'busyblocks-lead', tab: 1 });
    const seat = { org: 'Busytools', project: 'busyblocks', label: 'lead' };
    expect(await openSurface(null, seat)).toBeNull();
    expect(surface.profile, "the shell's answer is the raised profile").toBe('busyblocks-lead');
    hide.mockResolvedValue(true);
    await closeSurface();
    expect(hide, 'Done lowers that browser, not the shared one').toHaveBeenCalledWith(
      'busyblocks-lead',
      null,
    );
  });

  it("a refused hide leaves the surface's failure drawn rather than claiming it closed", async () => {
    show.mockResolvedValue({ profile: 'hunt', tab: 1 });
    await openSurface(null);
    hide.mockResolvedValue('the browser host is gone');
    expect(await closeSurface()).toBe('the browser host is gone');
    expect(surface.phase).toBe('failed');
    expect(surface.reason).toBe('the browser host is gone');
  });
});

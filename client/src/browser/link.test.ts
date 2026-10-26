import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { BrowserFrame, Connection, ConnectionStatus } from '../socket';
import {
  canAttach,
  drawsCanvas,
  drawsSurface,
  hideBrowser,
  installBrowserLink,
  keptFrame,
  listTabs,
  navigateBrowser,
  onTabs,
  onView,
  resizeBrowser,
  showBrowser,
  sizesAsWatcher,
  takeControl,
} from './link';

/** A connection that speaks only the browser's own messages. */
function fakeConnection() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  const frames = new Set<(frame: BrowserFrame) => void>();
  const wire = {
    browserView: vi.fn<(seat: unknown, tab: number | null) => Promise<unknown>>(() =>
      Promise.resolve({ profile: 'hunt', tab: 2 }),
    ),
    browserTabs: vi.fn<() => Promise<unknown>>(() =>
      Promise.resolve({
        profile: 'hunt',
        id: 1,
        up: true,
        connected: true,
        closing_at_ms: null,
        rows: [],
      }),
    ),
    browserDetach: vi.fn(() => true),
    browserControl: vi.fn(() => true),
    browserNav: vi.fn(() => true),
    browserResize: vi.fn(() => true),
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    onBrowserFrame: (fn: (frame: BrowserFrame) => void) => {
      frames.add(fn);
      return () => frames.delete(fn);
    },
  };
  return {
    wire,
    /** One frame off the socket, as the server relays it. */
    frame(frame: BrowserFrame) {
      for (const fn of frames) fn(frame);
    },
    /** One server message, as the socket decoded it. */
    message(message: ServerMessage) {
      for (const fn of listeners) fn(message);
    },
    status(status: ConnectionStatus) {
      for (const fn of statuses) fn(status);
    },
  };
}

const SEAT = { org: 'Busytools', project: 'forge', label: 'lead' };

function install() {
  const fake = fakeConnection();
  installBrowserLink(fake.wire as unknown as Connection);
  return fake;
}

beforeEach(() => {
  vi.restoreAllMocks();
});

describe('the wire link', () => {
  /// **A page with no shell draws the takeover itself.** A change that made
  /// this false would hide the surface on the web again.
  it('draws the surface and its own canvas outside the shell', () => {
    install();
    expect(canAttach()).toBe(true);
    expect(drawsSurface()).toBe(true);
    expect(drawsCanvas()).toBe(true);
  });

  /// **The attach names the seat and the tab the hand-off asked for**, and
  /// answers the pair the server resolved.
  it('attaches through the seat and tab, and answers the resolved pair', async () => {
    const fake = install();
    expect(await showBrowser(null, SEAT, 4)).toEqual({ profile: 'hunt', tab: 2 });
    expect(fake.wire.browserView).toHaveBeenCalledWith(SEAT, 4);
  });

  /// **A refused attach is the server's own sentence**, not a made-up one.
  it('carries the server sentence when it refuses the attach', async () => {
    const fake = install();
    fake.wire.browserView.mockRejectedValueOnce(new Error('no tab 9'));
    expect(await showBrowser(null, SEAT, 9)).toBe('no tab 9');
  });

  /// **A page attaches to a seat, never to a nameless profile**: the wire
  /// names the seat, so a hand-off with none on the web is refused here.
  it('refuses an attach with no seat to name', async () => {
    const fake = install();
    expect(await showBrowser('hunt', null, 1)).toMatch(/no seat/);
    expect(fake.wire.browserView).not.toHaveBeenCalled();
  });

  /// **Lowering is a detach**, never a command that reaches the core.
  it('lowers by detaching the attached profile', async () => {
    const fake = install();
    expect(await hideBrowser('hunt', SEAT)).toBe(true);
    expect(fake.wire.browserDetach).toHaveBeenCalledTimes(1);
  });

  /// **A connection that went is said, not claimed closed.**
  it('answers the connection sentence when the detach could not go', async () => {
    const fake = install();
    fake.wire.browserDetach.mockReturnValueOnce(false);
    expect(typeof (await hideBrowser('hunt', SEAT))).toBe('string');
  });

  /// **The page is laid out to the controller's surface in device pixels**,
  /// which is what keeps a gesture on the field it touched.
  it('sizes the page in the device pixels it was handed', async () => {
    const fake = install();
    await resizeBrowser(1600, 1200, 2);
    expect(fake.wire.browserResize).toHaveBeenCalledWith(1600, 1200, 2);
  });

  /// **A page with no shell is the client that sizes its own view**, without
  /// taking the controller role.
  it('sizes the page as a watcher, unlike the shell', () => {
    install();
    expect(sizesAsWatcher(), 'the page lays itself out').toBe(true);
  });

  /// **A size the page already has is not a request**: a re-measure that lands
  /// on the same box must not force the host through another layout. An
  /// attach forgets the cache, because another client may have moved it.
  it('asks once for one size, and re-asks after an attach', async () => {
    const fake = install();
    await resizeBrowser(1600, 1200, 2);
    await resizeBrowser(1600, 1200, 2);
    expect(fake.wire.browserResize, 'the same size asks nothing twice').toHaveBeenCalledTimes(1);

    await showBrowser(null, SEAT, 1);
    await resizeBrowser(1600, 1200, 2);
    expect(fake.wire.browserResize, 'an attach is a fresh ask').toHaveBeenCalledTimes(2);
  });

  /// **The arrows are the wire's own move**, and back is the named direction.
  it('moves the page through its history as a nav message', async () => {
    const fake = install();
    await navigateBrowser('back');
    await navigateBrowser('forward');
    expect(fake.wire.browserNav.mock.calls).toEqual([[true], [false]]);
  });

  /// **The role is the server's to record**, so taking control is the message
  /// and never a local flag.
  it('takes the controller role through the wire', async () => {
    const fake = install();
    await takeControl(true);
    expect(fake.wire.browserControl).toHaveBeenCalledWith(true);
  });

  /// **A tab list names a seat**, and a page with none has no profile to list.
  it('refuses a tab list with no seat to name', async () => {
    const fake = install();
    await expect(listTabs(null)).rejects.toThrow(/no seat/);
    expect(fake.wire.browserTabs, 'nothing malformed goes on the wire').not.toHaveBeenCalled();
  });

  /// **The tabs a page reads are narrowed on the way in**, so a malformed
  /// list fails the read rather than drawing an empty strip.
  it('reads the tabs and refuses a list it cannot read', async () => {
    const fake = install();
    expect((await listTabs(SEAT)).rows).toEqual([]);
    fake.wire.browserTabs.mockResolvedValueOnce({ profile: 'hunt' });
    await expect(listTabs(SEAT)).rejects.toThrow(/host is up|names no profile/);
  });

  /// **A frame that landed before the canvas mounted is still paintable.**
  /// Without this a still page opened onto a blank canvas for good.
  it('keeps the newest relayed frame for a late canvas', async () => {
    const fake = install();
    const frame: BrowserFrame = {
      profile: 1,
      tab: 2,
      seq: 7,
      width: 10,
      height: 10,
      key: true,
      bytes: new Uint8Array([1]),
    };
    fake.frame(frame);
    expect(await keptFrame()).toBe(frame);
  });

  /// **The pushed viewer state and tab list reach the surface's own
  /// handlers**, so a page is driven by the server exactly as the shell is.
  it('folds the pushed viewer state and tab list', async () => {
    const fake = install();
    const seenView: Record<string, unknown>[] = [];
    const seenTabs: unknown[] = [];
    await onView((state) => seenView.push(state));
    await onTabs((payload) => seenTabs.push(payload));

    fake.message({ kind: 'browser_nav_state', id: 1, tab: 2, can_back: true, can_forward: false });
    fake.message({
      kind: 'browser_view',
      id: 1,
      profile: 'hunt',
      tab: 2,
      controller: 3,
      viewers: 1,
      viewer: 3,
    });
    fake.message({
      kind: 'browser_tabs',
      profile: 'hunt',
      id: 1,
      up: true,
      connected: true,
      closing_at_ms: null,
      rows: [],
    });

    expect(seenView.map((state) => state['tab'])).toEqual([2]);
    expect(seenTabs).toHaveLength(1);
  });
});

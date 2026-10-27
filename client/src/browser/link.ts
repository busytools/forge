/**
 * The browser's link: the one seam between what the page draws and what the
 * server's browser is driven through.
 *
 * **Two clients, one interface.** Inside the shell the browser is a native
 * overlay the Rust half paints, and the page measures the box it sits in;
 * a page with no shell has no native layer, so it draws the same frames on its
 * own canvas and its takeover is a box the page owns. Everything above this
 * file - the surface store, the strip, the dock, the painter - is the same
 * code either way. A platform check anywhere but here is the defect this
 * interface exists to delete.
 *
 * **A page with no connection draws no browser.** The link answers with its
 * own refusal rather than invoking something that is not there.
 */

import type { BrowserTabList } from '../protocol';
import type { BrowserFrame, BrowserWire, Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { canHost, type HideAnswer, type OpenAnswer } from './host';
import { shellLink } from './shell';
import { wireLink } from './wire';

/** What the surface store, the strip and the dock read and call. */
export interface BrowserLink {
  /** Whether this page can reach a browser at all. */
  readonly attachable: boolean;
  /** Whether this page draws the takeover: the shell's bar and states. */
  readonly drawsSurface: boolean;
  /** Whether the page paints the frames itself, on a canvas. */
  readonly paintsCanvas: boolean;
  /**
   * Whether this page lays the viewed page out to its own box while it is a
   * watcher.
   *
   * **A shell does not**: its watcher window must not move the page under the
   * person driving, and the server refuses it anyway. A page with no shell has
   * no other client behind it to size the view, so it does.
   */
  readonly sizesAsWatcher: boolean;
  /**
   * Whether this page re-attaches its surface itself when the connection
   * comes back.
   *
   * **A shell has a bridge that does it** on the socket it owns; a page's
   * attach lived on the socket that dropped, so the page asks again or the
   * surface keeps a picture no gesture can reach.
   */
  readonly reattachesOnReconnect: boolean;
  /**
   * Whether the takeover pushes a page-history entry so the browser's Back
   * lowers it.
   *
   * **A shell's Back is the shell's** - the window has its own hardware Back
   * path - while a browser tab's Back would otherwise leave the seat.
   */
  readonly usesPageHistory: boolean;
  show(profile: string | null, seat: SessionSlot | null, tab: number | null): Promise<OpenAnswer>;
  hide(profile: string | null, seat: SessionSlot | null): Promise<HideAnswer>;
  /** The page area's box, for a shell to place its native layer in. */
  reportRect(
    rect: { x: number; y: number; width: number; height: number },
    viewport: { width: number; height: number },
    scale: number,
  ): Promise<void>;
  /** Lay the page out to this surface, in device pixels and the ratio. */
  resize(width: number, height: number, scale: number): Promise<void>;
  navigate(direction: 'back' | 'forward'): Promise<void>;
  takeControl(take: boolean): Promise<void>;
  /** The newest frame the shell kept, for a canvas that mounts after it. */
  keptFrame(): Promise<BrowserFrame | null>;
  listTabs(seat: SessionSlot | null): Promise<BrowserTabList>;
  /** The pushed tab list, already narrowed; a list this page cannot read is
   *  said on the console rather than handed on as one. */
  onTabs(fn: (list: BrowserTabList) => void): Promise<() => void>;
  onConnection(fn: () => void): Promise<() => void>;
  /** The attach's own state: the profile id, the tab, the role holders. */
  onView(fn: (state: Record<string, unknown>) => void): Promise<() => void>;
  /** The viewed page's history, as the arrows may move it. */
  onNav(fn: (flags: Record<string, unknown>) => void): Promise<() => void>;
  /** The viewed tab's browser stopped, and why. */
  onFailed(fn: (payload: unknown) => void): Promise<() => void>;
  /** The shell's keyboard way out; a page has its own doors. */
  onLower?: (fn: () => void) => Promise<() => void>;
  /** The shell's re-measure ask; a page's layout is its own box. */
  onRemeasure?: (fn: () => void) => Promise<() => void>;
}

/** A page with no connection: every ask answers the same refusal. */
const OFFLINE: BrowserLink = {
  attachable: false,
  drawsSurface: false,
  paintsCanvas: false,
  sizesAsWatcher: false,
  reattachesOnReconnect: false,
  usesPageHistory: false,
  show: () => Promise.resolve('this page is not connected'),
  hide: () => Promise.resolve('this page is not connected'),
  reportRect: () => Promise.resolve(),
  resize: () => Promise.resolve(),
  navigate: () => Promise.resolve(),
  takeControl: () => Promise.resolve(),
  keptFrame: () => Promise.resolve(null),
  listTabs: () => Promise.reject(new Error('this page is not connected')),
  onTabs: () => Promise.resolve(() => {}),
  onConnection: () => Promise.resolve(() => {}),
  onView: () => Promise.resolve(() => {}),
  onNav: () => Promise.resolve(() => {}),
  onFailed: () => Promise.resolve(() => {}),
};

let active: BrowserLink | null = null;

/** The link this page draws through. */
export function browserLink(): BrowserLink {
  return active ?? OFFLINE;
}

/**
 * Choose the link for this connection and make it the one every surface uses.
 *
 * **The only platform check in the browser code**: a page inside the shell
 * hands its browser to the native overlay, and a plain page drives it over
 * the socket it already has.
 */
export function installBrowserLink(connection: Connection): void {
  active = canHost() ? shellLink() : hasWire(connection) ? wireLink(connection) : OFFLINE;
}

/** Give the link up with the connection it was made for. */
export function clearBrowserLink(): void {
  active = null;
}

/** Whether this connection carries the browser's own client messages. */
function hasWire(connection: Connection): connection is Connection & BrowserWire {
  const held = connection as unknown as Partial<BrowserWire>;
  return typeof held.browserView === 'function' && typeof held.browserTabs === 'function';
}

// **The surface, the strip and the dock call these, never a platform check.**
export function canAttach(): boolean {
  return browserLink().attachable;
}

export function drawsSurface(): boolean {
  return browserLink().drawsSurface;
}

export function drawsCanvas(): boolean {
  return browserLink().paintsCanvas;
}

/** Whether this page sizes the viewed page while it is only watching. */
export function sizesAsWatcher(): boolean {
  return browserLink().sizesAsWatcher;
}

/** Whether this page asks for its attach again when the socket comes back. */
export function reattachesOnReconnect(): boolean {
  return browserLink().reattachesOnReconnect;
}

/** Whether the takeover is lowered by the browser's own Back. */
export function usesPageHistory(): boolean {
  return browserLink().usesPageHistory;
}

export function showBrowser(
  profile: string | null,
  seat: SessionSlot | null,
  tab: number | null,
): Promise<OpenAnswer> {
  return browserLink().show(profile, seat, tab);
}

export function hideBrowser(
  profile: string | null = null,
  seat: SessionSlot | null = null,
): Promise<HideAnswer> {
  return browserLink().hide(profile, seat);
}

export function reportSurfaceRect(
  rect: { x: number; y: number; width: number; height: number },
  viewport: { width: number; height: number },
  scale: number,
): Promise<void> {
  return browserLink().reportRect(rect, viewport, scale);
}

export function resizeBrowser(width: number, height: number, scale: number): Promise<void> {
  return browserLink().resize(width, height, scale);
}

export function navigateBrowser(direction: 'back' | 'forward'): Promise<void> {
  return browserLink().navigate(direction);
}

export function takeControl(take: boolean): Promise<void> {
  return browserLink().takeControl(take);
}

export function keptFrame(): Promise<BrowserFrame | null> {
  return browserLink().keptFrame();
}

export function listTabs(seat: SessionSlot | null = null): Promise<BrowserTabList> {
  return browserLink().listTabs(seat);
}

export function onTabs(fn: (list: BrowserTabList) => void): Promise<() => void> {
  return browserLink().onTabs(fn);
}

export function onConnection(fn: () => void): Promise<() => void> {
  return browserLink().onConnection(fn);
}

export function onView(fn: (state: Record<string, unknown>) => void): Promise<() => void> {
  return browserLink().onView(fn);
}

export function onNav(fn: (flags: Record<string, unknown>) => void): Promise<() => void> {
  return browserLink().onNav(fn);
}

export function onFailed(fn: (payload: unknown) => void): Promise<() => void> {
  return browserLink().onFailed(fn);
}

/** The shell's keyboard way out, or `null` where the link has no such door. */
export function onLower(fn: () => void): Promise<() => void> | null {
  const start = browserLink().onLower;
  return start === undefined ? null : start(fn);
}

/** The shell's re-measure ask, or `null` where the page's box is its own. */
export function onRemeasure(fn: () => void): Promise<() => void> | null {
  const start = browserLink().onRemeasure;
  return start === undefined ? null : start(fn);
}

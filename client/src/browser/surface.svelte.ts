/**
 * The browser surface: the full-window takeover the person's Open opens, and
 * every state between it and the chat they go back to.
 *
 * **One surface at a time, and the host outlives it.** Going back to the
 * chat is not closing the browser: the sessions keep driving the same page,
 * and the next Open shows it exactly as they left it.
 *
 * **The takeover's answers are the seat's hand-off's.** The bar carries back
 * to chat whenever this surface is up; Done, Not now and the feedback line are
 * drawn by the composer that owns the ask, and only while this surface is the
 * page that ask named: the seat it was raised for and the tab it shows are
 * what a hand-off on this page is answered from.
 *
 * A superseded open never lands: the generation is read after every await,
 * so a second Open (or a close) while the first is still starting leaves the
 * store on the LATEST request, not the slowest one.
 */

import type { BrowserTabList } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { whyText } from './host';
import {
  canAttach,
  clearBrowserLink,
  drawsSurface,
  hideBrowser,
  installBrowserLink,
  listTabs,
  onConnection,
  onFailed,
  onLower,
  onNav,
  onRemeasure,
  onTabs,
  onView,
  reattachesOnReconnect,
  reportSurfaceRect,
  resizeBrowser,
  showBrowser,
  sizesAsWatcher,
  takeControl as sendControl,
  usesPageHistory,
} from './link';

export type SurfacePhase = 'closed' | 'opening' | 'live' | 'failed';

/** What the open page's own history allows, as the shell reads it. */
export interface NavState {
  can_back: boolean;
  can_forward: boolean;
}

interface Surface {
  phase: SurfacePhase;
  /** The name the person opened, `null` for the shared browser. */
  profile: string | null;
  /** The shell's sentence when the surface could not open. */
  reason: string | null;
  /**
   * The viewed profile's wire id, from the shell's `browser_view`: **the
   * frames a phone paints are filtered by it**, so one relayed for another
   * profile never lands on this canvas.
   */
  id: number | null;
  /**
   * The viewed tab, the other half of the frame filter: a frame for another
   * tab never paints, whichever profile it names.
   */
  tab: number | null;
  /**
   * The seat whose browser this is, kept for a tab switch: a re-attach names
   * the same seat and the new tab.
   */
  seat: SessionSlot | null;
  /**
   * The viewed profile's tabs, for the bar's picker. The strip reads its own
   * copy; this one follows the surface, so the picker cannot list a profile
   * the page is no longer showing.
   */
  tabs: BrowserTabList | null;
  /**
   * The profile the server actually attached, from `browser_view` - the
   * requested name resolved (the seat's own when none was named). **The bar
   * spells it**, so two devices looking at two different browsers cannot read
   * as one browser with a shared role.
   */
  attachedProfile: string | null;
  /** The bar's two history arrows; both disabled until the shell answers. */
  nav: NavState;
  /**
   * This connection's own viewer number, from the shell's `browser_view`.
   * The server names it, so the bar can tell whose role it is reading.
   */
  viewer: number | null;
  /** The viewer that holds the controller role; `null` when nobody does. */
  controller: number | null;
  /**
   * The dock's bar height, which the page area starts under.
   *
   * **The bar is DOM and the page is a native layer above the webview**, so
   * anything the bar grows by (the feedback row, a take's card) has to move
   * the page's rect down or the bar is drawn underneath the picture.
   */
  barHeight: number;
}

export const surface: Surface = $state({
  phase: 'closed',
  profile: null,
  reason: null,
  id: null,
  tab: null,
  seat: null,
  tabs: null,
  attachedProfile: null,
  nav: { can_back: false, can_forward: false },
  viewer: null,
  controller: null,
  barHeight: 38,
});

/**
 * A platform Back asks this first: a live takeover is its door, and the answer
 * says whether the Back was consumed. It goes the same way every lower does -
 * the `forge-takeover` word the dock and the strip already fold - so the
 * hand-off is left parked exactly as back-to-chat leaves it.
 */
if (typeof window !== 'undefined') {
  (window as unknown as { __forgeLower?: () => boolean }).__forgeLower = (): boolean => {
    if (surface.phase === 'closed') return false;
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'lowered' }));
    return true;
  };
  // **Every lower lands here, whichever door asked for it.** The hardware
  // Back's `__forgeLower` dispatches the word, and it goes through the same
  // close the dock and the strip call - the shell's view is given up and the
  // window comes back, not just the store's phase.
  window.addEventListener('forge-takeover', (event: Event) => {
    if ((event as CustomEvent<string>).detail !== 'lowered') return;
    if (surface.phase === 'closed') return;
    void closeSurface();
  });
}

/**
 * Whether this takeover pushed a page-history entry of its own.
 *
 * **A browser tab's Back lowers the takeover** - rule 22's hardware Back, for
 * a tab: opening pushes an entry, and a Back while the surface is up lowers it
 * instead of leaving the seat. The UI's own lower pops the entry, so a Back
 * after Done does not land on a takeover that is already down.
 */
let pushed = false;

function pushHistory(): void {
  if (!usesPageHistory() || pushed) return;
  pushed = true;
  // The DOM types `history.state` as any; it is this page's own entry, and a
  // non-object one is replaced by the takeover's alone.
  const held: unknown = history.state;
  const base = typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
  history.pushState({ ...base, forgeTakeover: true }, '');
}

function popHistory(): void {
  if (!pushed) return;
  // Cleared first: the popstate that follows finds a closed surface and adds
  // nothing, rather than lowering a second time.
  pushed = false;
  history.back();
}

if (typeof window !== 'undefined') {
  window.addEventListener('popstate', () => {
    if (!pushed) return;
    pushed = false;
    if (surface.phase === 'closed') return;
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: 'lowered' }));
  });
}

/** Whether this surface's input will reach the page: the controller role. */
export function holdsControl(): boolean {
  return surface.viewer !== null && surface.controller === surface.viewer;
}

/**
 * Take the controller role for this surface, or give it back.
 *
 * **The role is the server's to give, and its `browser_view` push is what
 * settles it.** A take it refuses - no attach, a tab that moved - must leave
 * the bar saying this surface does not hold it, or the page shows a driver
 * whose every gesture the server then refuses. A release is the one this side
 * can settle itself: the state it claims is the one nobody holds.
 */
export async function takeControl(take: boolean): Promise<void> {
  if (!drawsSurface()) return;
  await sendControl(take);
  if (!take) surface.controller = null;
  if (take) void resizeToRegion();
}

/** The request that is current; a result from an older one is dropped. */
let generation = 0;

/** The newest mount token. **A mount claims one**, so only a newer mount
 *  makes an older instance's teardown stale; a retry or a tab pick is not a
 *  new mount and must not. */
let mounts = 0;

/** Claim a mount token for one takeover component. */
export function claimSurfaceMount(): number {
  mounts += 1;
  return mounts;
}

/** Whether `token` is the newest mount's. */
export function surfaceMountIsCurrent(token: number): boolean {
  return token === mounts;
}

/**
 * Open the takeover for one of a seat's browser tabs.
 *
 * `profile` is the name the row already knows, drawn while the attach is in
 * flight; `tab` is the tab to show, and `null` the seat's current one (else
 * the profile's first). Answers why it could not open, or `null` when it did.
 */
export async function openSurface(
  profile: string | null,
  seat: SessionSlot | null = null,
  tab: number | null = null,
): Promise<string | null> {
  // **A page with no connection draws no browser.** The refusal belongs to
  // the strip's row, not to a full-screen error.
  if (!canAttach()) return 'this page is not the client';
  const mine = ++generation;
  const here = drawsSurface();
  if (here) {
    surface.phase = 'opening';
    surface.profile = profile;
    surface.reason = null;
    // **A fresh page has nowhere to go until the shell says otherwise.**
    surface.nav = { can_back: false, can_forward: false };
    surface.id = null;
    surface.tab = tab;
    surface.seat = seat;
    surface.tabs = null;
    surface.attachedProfile = null;
    surface.viewer = null;
    surface.controller = null;
    // The link's install is the normal one; this retries it after a listener
    // that was refused then, so a rejected fetch cannot cost the page its
    // subscriptions for life. Idempotent, and started before the show so the
    // first open already has the listeners a refusal asks through.
    void installSurfaceEvents();
  }
  const answer = await showBrowser(profile, seat, tab);
  const why = typeof answer === 'string' ? answer : null;
  // **The phone's takeover is the native one**: its shell raised it, and the
  // page's components are the desktop's.
  if (!here) return why;
  if (mine !== generation) {
    // A stale open that landed after a close still has a surface up in the
    // shell; it is taken down rather than left drawing under a closed store.
    if (typeof answer !== 'string' && surface.phase === 'closed') {
      void hideBrowser(answer.profile);
    }
    return why;
  }
  if (typeof answer === 'string') {
    surface.phase = 'failed';
    surface.reason = answer;
    return answer;
  }
  // **The shell's answer is the profile and tab it attached**, which a
  // hand-off with no tab resolves to the asking seat's current one.
  surface.profile = answer.profile;
  surface.tab = answer.tab;
  surface.phase = 'live';
  surface.reason = null;
  pushHistory();
  // **The picker is an aid, not the open**: a slow read must not delay the
  // caller that is waiting to hear the raise landed.
  void readSurfaceTabs(seat);
  return null;
}

/**
 * Move the viewed surface to another tab: a re-attach on the same seat.
 *
 * The frame stream switches with the attach, and `browser_view` pushes the
 * tab it landed on - so this only starts the move and reads its answer.
 */
export async function selectTab(tab: number | null): Promise<string | null> {
  if (!canAttach()) return 'this page is not the client';
  // **A failed surface can still pick a live tab**: the page it was showing
  // may be gone while the profile's other tabs are not.
  if ((surface.phase !== 'live' && surface.phase !== 'failed') || surface.seat === null) {
    return 'no browser is open';
  }
  // **A pick is a surface move like any other**, so it takes a generation:
  // an answer that lands after a close, a newer open or a later pick is not
  // this call's to apply.
  const mine = ++generation;
  const answer = await showBrowser(surface.profile, surface.seat, tab);
  if (typeof answer === 'string') {
    // A superseded refusal is not the current surface's to fail.
    if (mine !== generation) return answer;
    surface.phase = 'failed';
    surface.reason = answer;
    // **A pick that failed left the dead connection's role and arrows
    // behind**: nothing is attached to be driven, and no page is there to
    // move through, so the bar draws neither.
    surface.viewer = null;
    surface.controller = null;
    surface.nav = { can_back: false, can_forward: false };
    return answer;
  }
  if (mine !== generation) return null;
  surface.profile = answer.profile;
  surface.tab = answer.tab;
  // **A successful pick is a live surface again.** Attaching from the failed
  // state used to leave the phase (and the reason) behind, so the phone never
  // painted and the desktop controlled an overlay its bar called failed.
  surface.phase = 'live';
  surface.reason = null;
  void readSurfaceTabs(surface.seat);
  return null;
}

/** The token of the newest picker read; an older answer must not overwrite it. */
let tabsRead = 0;

/** The profile's tabs, read for the bar's picker; a failed read leaves the last. */
async function readSurfaceTabs(seat: SessionSlot | null): Promise<void> {
  const token = (tabsRead += 1);
  try {
    const list = await listTabs(seat);
    // **A slower read must not replace a newer one's rows**, and a list for
    // another profile must never become this picker's: a pick from it would
    // ask for a tab number that belongs to a browser the page is not on.
    if (token !== tabsRead) return;
    if (surface.attachedProfile !== null && list.profile !== surface.attachedProfile) return;
    if (surface.phase !== 'closed') surface.tabs = list;
  } catch (why) {
    if (token !== tabsRead) return;
    // The picker is an aid, not the surface: a read that could not run is
    // said in the console and the last list stays drawn.
    console.warn(`the browser's tabs could not be read: ${whyText(why)}`);
  }
}

/**
 * Take the surface away; the browser keeps running. `profile` names the one
 * to lower - a row knows its own name even when this page's store was reset
 * by a reload while the shell's surface stayed up. **`seat` is the asking
 * hand-off's when `profile` is null**: the raise resolved it to that seat's
 * own profile, and Done must lower the same browser. Answers why it could
 * not, or `null` when it is closed.
 */
export async function closeSurface(
  profile: string | null = surface.profile,
  seat: SessionSlot | null = null,
): Promise<string | null> {
  const mine = ++generation;
  const answer = await hideBrowser(profile, seat);
  // **A lower superseded by a newer open is not this surface's to close.**
  if (mine !== generation) return null;
  if (typeof answer === 'string') {
    surface.phase = 'failed';
    surface.reason = answer;
    return answer;
  }
  // **A lower has two answers**: the shell's sentence or a taken-down page.
  // A string was returned above, so this is the closed state.
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  surface.nav = { can_back: false, can_forward: false };
  surface.id = null;
  surface.tab = null;
  surface.seat = null;
  surface.tabs = null;
  surface.attachedProfile = null;
  surface.viewer = null;
  surface.controller = null;
  popHistory();
  return null;
}

/** The link's own pushes, as steps; `null` is a door this link has not. */
interface Step {
  what: string;
  start: () => Promise<() => void> | null;
}

/**
 * What the surface follows while it is up: the failure state, the keyboard's
 * way out, the remeasure ask, the roles, and the page's history as it moves.
 *
 * **The order is the shell's**, and the counter below makes an install that
 * was refused resumable: a listener that failed is retried from where it
 * stopped, never from the start.
 */
const STEPS: Step[] = [
  {
    what: "the keyboard's way out",
    start: () =>
      onLower(() => {
        // **The keyboard's way out.** The overlay holds first responder while
        // the surface is up; Cmd+W arrives here, and the page lowers the
        // surface the same way back-to-chat does - the hand-off stays parked
        // for the next answer.
        void closeSurface();
      }),
  },
  {
    what: 'the remeasure ask',
    start: () =>
      onRemeasure(() => {
        // **A refused report is measured again on the next frame.** The page
        // that measured a transient layout is the only one that can read the
        // settled one, and no ResizeObserver fires when only the position
        // changed.
        window.dispatchEvent(new Event('forge-surface-remeasure'));
      }),
  },
  { what: 'a failure', start: () => onFailed(failed) },
  { what: 'the history', start: () => onNav(nav) },
  { what: 'the viewer state', start: () => onView(viewed) },
  { what: 'the tabs', start: () => onTabs(tabs) },
  { what: 'the connection', start: () => onConnection(reconnect) },
];

/** How many steps are registered; all of them is a surface that follows. */
let installed = 0;
/** The attempt in flight, so callers arriving mid-install wait for it. */
let inFlight: Promise<void> | null = null;
/** Bumped by a new link, so a stale install cannot count its steps. */
let armed = 0;
/** The stops of the listeners already up, replaced with the link. */
const stops: Array<() => void> = [];

/**
 * Install the link for this connection and re-arm the surface's listeners.
 *
 * **A new connection is a new link, and a new set of pushes**: the listeners
 * that folded the old connection's frames are let go rather than left folding
 * another socket's.
 */
export function installBrowser(connection: Connection): void {
  installBrowserLink(connection);
  releaseEvents();
  if (drawsSurface()) void installSurfaceEvents();
}

/** Give the link up with the connection it was made for. */
export function uninstallBrowser(): void {
  releaseEvents();
  clearBrowserLink();
}

function releaseEvents(): void {
  armed += 1;
  inFlight = null;
  for (const stop of stops.splice(0)) {
    try {
      stop();
    } catch {
      // A listener the platform already dropped is not a surface's problem.
    }
  }
  installed = 0;
}

/**
 * The link's pushes while the surface is up.
 *
 * **The counter is what makes it resumable**: a listener that failed is
 * retried on the next call, from where it stopped, never from the start. The
 * in-flight promise is what keeps two callers from running the steps at once
 * - an open landing during the load-time attempt waits for it and then takes
 * it up, rather than being dismissed by it.
 */
export async function installSurfaceEvents(): Promise<void> {
  if (installed >= STEPS.length || !canAttach()) return;
  while (inFlight !== null) {
    await inFlight;
    if (installed >= STEPS.length) return;
  }
  inFlight = installOnce();
  try {
    await inFlight;
  } finally {
    inFlight = null;
  }
}

async function installOnce(): Promise<void> {
  const mine = armed;
  while (installed < STEPS.length) {
    const step = STEPS[installed];
    if (step === undefined) return;
    // A link with no such door (a page has no Cmd+W) skips the step rather
    // than stopping the install.
    const started = step.start();
    if (started === null) {
      installed += 1;
      continue;
    }
    try {
      const stop = await started;
      if (mine !== armed) {
        // A newer link replaced this one while the listener was registering:
        // let this registration go with it.
        if (typeof stop === 'function') stop();
        return;
      }
      if (typeof stop === 'function') stops.push(stop);
    } catch {
      // No event API (a plain page, a test harness), or one listener refused:
      // the count stays where it got to, and the next call resumes there.
      return;
    }
    installed += 1;
  }
}

/** **A failure after the surface closed is nobody's.** It must not re-mount
 *  the store as failed. */
function failed(payload: unknown): void {
  if (surface.phase === 'closed') return;
  let why = 'the browser stopped answering';
  if (typeof payload === 'string' && payload !== '') {
    why = payload;
  } else if (typeof payload === 'object' && payload !== null) {
    const fields = payload as Record<string, unknown>;
    const id = typeof fields['id'] === 'number' ? fields['id'] : null;
    const tab = typeof fields['tab'] === 'number' ? fields['tab'] : null;
    if (id !== null && tab !== null) {
      if (surface.id !== null && surface.tab !== null) {
        // **A failure that names another surface is not this one's**: a late
        // frame for a superseded attach must not fail the tab now up.
        if (id !== surface.id || tab !== surface.tab) return;
      } else if (surface.phase !== 'live') {
        // **An unknown pair is unfilterable only on a live surface.** While
        // opening, the pair is still the previous attach's, and this frame may
        // be that one's to fail.
        return;
      }
    }
    if (typeof fields['reason'] === 'string' && fields['reason'] !== '') {
      why = fields['reason'];
    }
  }
  surface.phase = 'failed';
  surface.reason = why;
  // There is no page left to move: the arrows go back to disabled.
  surface.nav = { can_back: false, can_forward: false };
  // The shell's surface is gone with the host; the page's keeps the sentence
  // up until the person answers the hand-off.
  void hideBrowser(surface.profile);
}

/** **The arrows are the server's reading of the page's history**, pushed on
 *  every committed navigation, so the bar never guesses what it can do. */
function nav(flags: Record<string, unknown>): void {
  surface.nav = {
    can_back: flags['can_back'] === true,
    can_forward: flags['can_forward'] === true,
  };
}

/**
 * **The role's truth is the server's push**, so two surfaces draw the same
 * answer about who is driving; `viewer` is this connection's own number, which
 * is what makes "take control" a read rather than a guess.
 */
function viewed(state: Record<string, unknown>): void {
  surface.id = typeof state['id'] === 'number' ? state['id'] : null;
  // **The tab is the other half of the frame filter**, and a re-attach pushes
  // the tab it landed on rather than the one this page asked for.
  surface.tab = typeof state['tab'] === 'number' ? state['tab'] : surface.tab;
  surface.viewer = typeof state['viewer'] === 'number' ? state['viewer'] : null;
  surface.controller = typeof state['controller'] === 'number' ? state['controller'] : null;
  // **The server resolves the name**, so a null request reads as the seat's
  // own profile.
  if (typeof state['profile'] === 'string') surface.attachedProfile = state['profile'];
}

/**
 * **The bar's picker follows the pushed list**, so a tab opening or closing
 * under the person moves the picker with it. A list for another profile is not
 * this surface's.
 */
function tabs(list: BrowserTabList): void {
  if (surface.attachedProfile !== null && list.profile !== surface.attachedProfile) return;
  surface.tabs = list;
}

/**
 * **A subscribe lives with its socket.** A reconnect starts a new one, so the
 * picker re-reads the tabs it was following rather than freezing on the last
 * list the dead socket pushed.
 */
function reconnect(): void {
  if (surface.phase === 'closed') return;
  void readSurfaceTabs(surface.seat);
  // **A page's attach died with the socket.** The server dropped this viewer
  // with the connection it attached on, so the surface would keep a picture
  // no gesture could reach; a re-attach either restores it or fails it. A
  // shell's bridge does the same on the socket it owns.
  if (reattachesOnReconnect() && surface.seat !== null) void selectTab(surface.tab);
}

/** The last page area measured, for the size the server lays the page out to. */
let region: { width: number; height: number; scale: number } | null = null;

/**
 * The takeover's own page area, in CSS pixels: the size this surface asks the
 * server to lay the page out at, and the page a gesture falls back to before
 * any frame names one.
 *
 * **A painted frame's page is the frame's own** (`BrowserFrame.page_width`),
 * not this: the page may be laid out for another surface, or resized by an
 * agent under the controller, and the picture is then letterboxed here.
 */
export function pageArea(): { width: number; height: number } | null {
  return region === null ? null : { width: region.width, height: region.height };
}

/** Ask the server for this surface's size: the controller's job, no one else's. */
function resizeToRegion(): Promise<void> {
  if (region === null) return Promise.resolve();
  const width = Math.max(1, Math.round(region.width * region.scale));
  const height = Math.max(1, Math.round(region.height * region.scale));
  return resizeBrowser(width, height, region.scale);
}

/** The takeover's page area, as the shell places the native layer: CSS pixels
 *  from the viewport's top-left, the page's own viewport, the device pixel
 *  ratio drawn at. */
export function setRegion(
  rect: { x: number; y: number; width: number; height: number },
  viewport: { width: number; height: number },
  scale: number,
): void {
  region = { width: rect.width, height: rect.height, scale };
  void reportSurfaceRect(rect, viewport, scale);
  // **A page with no shell is the only client behind its own view**, so it
  // lays the page out while it is only watching; a shell's watcher stays
  // quiet, because the server refuses it under a held role anyway.
  if (holdsControl() || sizesAsWatcher()) void resizeToRegion();
}

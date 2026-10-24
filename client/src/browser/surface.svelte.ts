/**
 * The browser surface: the full-window takeover the person's Open opens, and
 * every state between it and the chat they go back to.
 *
 * **One surface at a time, and the host outlives it.** Going back to the
 * chat is not closing the browser: the sessions keep driving the same page,
 * and the next Open shows it exactly as they left it.
 *
 * **The takeover's actions are the hand-off's.** The bar that carries Done,
 * Not now, the feedback line and back-to-chat is drawn by the dock that owns
 * the ask; the dock marks the takeover as live for that ask when Open is
 * pressed (`attachHandOff`), which is what routes a take's words to the
 * bar's field.
 *
 * A superseded open never lands: the generation is read after every await,
 * so a second Open (or a close) while the first is still starting leaves the
 * store on the LATEST request, not the slowest one.
 */

import type { SessionSlot } from '../wire/types';
import {
  canHost,
  drawsSurface,
  hideBrowser,
  reportSurfaceRect,
  resizeBrowser,
  showBrowser,
  takeControl as sendControl,
} from './host';

export type SurfacePhase = 'closed' | 'opening' | 'live' | 'failed';

/** The parked hand-off the takeover is standing for. */
export interface HandOff {
  id: string;
  profile: string | null;
}

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
   * The profile the server actually attached, from `browser_view` - the
   * requested name resolved (`@shared`, or the seat's own when none was
   * named). **The bar spells it**, so two devices looking at two different
   * browsers cannot read as one browser with a shared role.
   */
  attachedProfile: string | null;
  /** The hand-off this takeover answers, while one is parked. */
  hand_off: HandOff | null;
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
  /**
   * Which open this surface is: a fresh number per Open, carried with every
   * page-area report so the shell can refuse one measured for another open.
   *
   * **A webview reload resets this page's counter** while the shell keeps the
   * open it was told about; the page mounts no takeover after a reload until
   * the next Open, which re-mints both sides, and the shell's geometry check
   * bounds the mismatch in between.
   */
  epoch: number;
}

export const surface: Surface = $state({
  phase: 'closed',
  profile: null,
  reason: null,
  id: null,
  attachedProfile: null,
  hand_off: null,
  nav: { can_back: false, can_forward: false },
  viewer: null,
  controller: null,
  barHeight: 38,
  epoch: 0,
});

/**
 * The phone's hardware Back asks this first: a live takeover is its door,
 * and the answer says whether the Back was consumed. It goes the same way
 * every lower does - the `forge-takeover` word the dock and the strip already
 * fold - so the hand-off is left parked exactly as back-to-chat leaves it.
 * Installed by the shell alone; a plain page has no hardware Back to route.
 */
if (canHost() && typeof window !== 'undefined') {
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

/** Whether this surface's input will reach the page: the controller role. */
export function holdsControl(): boolean {
  return surface.viewer !== null && surface.controller === surface.viewer;
}

/**
 * Take the controller role for this surface, or give it back. **The reply's
 * state push is what settles it**; the store moves now so the bar does not
 * lag a round trip behind the person's press.
 */
export async function takeControl(take: boolean): Promise<void> {
  if (!drawsSurface()) return;
  await sendControl(take);
  surface.controller = take ? surface.viewer : null;
  if (take) void resizeToRegion();
}

/** Mark the takeover live for this hand-off, on Open. */
export function attachHandOff(hand_off: HandOff): void {
  surface.hand_off = hand_off;
}

/** Forget the hand-off once it is answered or its dock is gone. */
export function clearHandOff(id: string): void {
  if (surface.hand_off?.id === id) surface.hand_off = null;
}

/** The request that is current; a result from an older one is dropped. */
let generation = 0;

/**
 * Open the takeover for `profile` (`null` is the asking seat's own when a
 * seat rides along, and the shared browser for the person's own door; the
 * shared one is named `@shared`). Answers why it could not open, or `null`
 * when it did.
 */
export async function openSurface(
  profile: string | null,
  seat: SessionSlot | null = null,
): Promise<string | null> {
  // **The web page draws no takeover of its own.** Outside the shell the
  // failure belongs to the strip's row, not to a full-screen error.
  if (!canHost()) return 'this page is not the client';
  const mine = ++generation;
  surface.epoch = mine;
  const here = drawsSurface();
  if (here) {
    surface.phase = 'opening';
    surface.profile = profile;
    surface.reason = null;
    // **A fresh page has nowhere to go until the shell says otherwise.**
    surface.nav = { can_back: false, can_forward: false };
    surface.id = null;
    surface.attachedProfile = null;
    surface.viewer = null;
    surface.controller = null;
    // The load-time install is the normal one; this retries it after an
    // import that failed then, so a rejected fetch cannot cost the page its
    // subscriptions for life. Idempotent, and started before the show so the
    // first open already has the listeners a refusal asks through.
    void installSurfaceEvents();
  }
  const answer = await showBrowser(profile, seat, mine);
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
  // **The shell's answer is the profile it raised**, which a hand-off with no
  // profile resolves to the asking seat's own; Done lowers that one.
  surface.profile = answer.profile;
  surface.phase = 'live';
  return null;
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
  ++generation;
  const answer = await hideBrowser(profile, seat);
  if (typeof answer === 'string') {
    surface.phase = 'failed';
    surface.reason = answer;
    return answer;
  }
  // **The shell's answer decides the store.** A lower that took another
  // profile's page down is not this one's: the takeover stays live, with its
  // bar, over the page still on screen.
  if (answer.lowered || !drawsSurface()) {
    surface.phase = 'closed';
    surface.profile = null;
    surface.reason = null;
    surface.nav = { can_back: false, can_forward: false };
    surface.id = null;
    surface.attachedProfile = null;
    surface.viewer = null;
    surface.controller = null;
  } else {
    surface.phase = 'live';
  }
  return null;
}

/** How many of the shell's listeners are registered; five is all of them. */
let installed = 0;
/** The attempt in flight, so callers arriving mid-install wait for it. */
let inFlight: Promise<void> | null = null;

/**
 * The shell's own words while the surface is up: the failure state, the
 * keyboard's way out, the remeasure ask, and the page's history as it moves.
 *
 * **The counter is what makes it resumable**: a listener that failed is
 * retried on the next call, from where it stopped, never from the start. The
 * in-flight promise is what keeps two callers from running the steps at once
 * - an open landing during the load-time attempt waits for it and then takes
 * it up, rather than being dismissed by it.
 */
export async function installSurfaceEvents(): Promise<void> {
  if (installed === 5 || !canHost()) return;
  while (inFlight !== null) {
    await inFlight;
    if (installed === 5) return;
  }
  inFlight = installOnce();
  try {
    await inFlight;
  } finally {
    inFlight = null;
  }
}

async function installOnce(): Promise<void> {
  try {
    const { listen } = await import('@tauri-apps/api/event');
    if (installed < 1) {
      // **The keyboard's way out.** The overlay holds first responder while
      // the surface is up; Cmd+W arrives as this event, and the page lowers
      // the surface the same way back-to-chat does - the hand-off stays
      // parked for the bar's Done or Not now.
      await listen('browser_surface_lower', () => {
        void closeSurface();
      });
      installed = 1;
    }
    if (installed < 2) {
      // **A refused report is measured again on the next frame.** The shell
      // refused it because it was not this window's page area; the page that
      // measured a transient layout is the only one that can read the settled
      // one, and no ResizeObserver fires when only the position changed.
      await listen('browser_surface_remeasure', () => {
        window.dispatchEvent(new Event('forge-surface-remeasure'));
      });
      installed = 2;
    }
    if (installed < 3) {
      await listen<string>('browser_surface_failed', (event) => {
        const why =
          typeof event.payload === 'string' && event.payload !== ''
            ? event.payload
            : 'the browser stopped answering';
        surface.phase = 'failed';
        surface.reason = why;
        // There is no page left to move: the arrows go back to disabled.
        surface.nav = { can_back: false, can_forward: false };
        // The shell's surface is gone with the host; the page's keeps the
        // sentence up until the person answers the hand-off.
        void hideBrowser(surface.profile);
      });
      installed = 3;
    }
    if (installed < 4) {
      // **The arrows are the shell's reading of the page's history.** It is
      // pushed on every committed navigation, so the bar never guesses what
      // the page can do.
      await listen<Record<string, unknown>>('browser_nav_state', (event) => {
        const flags = event.payload;
        surface.nav = {
          can_back: flags.can_back === true,
          can_forward: flags.can_forward === true,
        };
      });
      installed = 4;
    }
    if (installed < 5) {
      // **The role's truth is the server's push**, so two surfaces draw the
      // same answer about who is driving; `viewer` is this connection's own
      // number, which is what makes "take control" a read rather than a
      // guess.
      await listen<Record<string, unknown>>('browser_view', (event) => {
        const state = event.payload;
        surface.id = typeof state.id === 'number' ? state.id : null;
        surface.viewer = typeof state.viewer === 'number' ? state.viewer : null;
        surface.controller = typeof state.controller === 'number' ? state.controller : null;
        // **The server resolves the name**, so a null request reads as the
        // seat's own profile rather than as the shared browser.
        if (typeof state.profile === 'string') surface.attachedProfile = state.profile;
      });
      installed = 5;
    }
  } catch {
    // No event API (a plain page, a test harness), or one listener refused:
    // the count stays where it got to, and the next call resumes there.
  }
}

/** The last page area measured, for the size the server lays the page out to. */
let region: { width: number; height: number; scale: number } | null = null;

/**
 * The page area's size, in the page's own CSS pixels.
 *
 * **A gesture maps through this, not through the canvas element**: the
 * picture is scaled to fit its box, and a letterboxed canvas is smaller than
 * the page it shows.
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
 *  ratio drawn at, and the epoch of the mount that measured it. */
export function setRegion(
  rect: { x: number; y: number; width: number; height: number },
  viewport: { width: number; height: number },
  scale: number,
  epoch: number,
): void {
  region = { width: rect.width, height: rect.height, scale };
  void reportSurfaceRect(rect, viewport, scale, epoch);
  if (holdsControl()) void resizeToRegion();
}

// **Installed at page load, before any surface can open.** The shell pushes
// the page's history as the surface attaches, and a listener installed on
// the far side of the open would miss that first answer.
if (drawsSurface()) void installSurfaceEvents();

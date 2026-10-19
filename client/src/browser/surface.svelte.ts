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
import { canHost, drawsSurface, hideBrowser, reportSurfaceRect, showBrowser } from './host';

export type SurfacePhase = 'closed' | 'opening' | 'live' | 'failed';

/** The parked hand-off the takeover is standing for. */
export interface HandOff {
  id: string;
  profile: string | null;
}

interface Surface {
  phase: SurfacePhase;
  /** The name the person opened, `null` for the shared browser. */
  profile: string | null;
  /** The shell's sentence when the surface could not open. */
  reason: string | null;
  /** The hand-off this takeover answers, while one is parked. */
  hand_off: HandOff | null;
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
  hand_off: null,
  barHeight: 38,
});

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
 * Open the takeover for `profile` (`null` is shared). Answers why it could
 * not open, or `null` when it did.
 */
export async function openSurface(
  profile: string | null,
  seat: SessionSlot | null = null,
): Promise<string | null> {
  // **The web page draws no takeover of its own.** Outside the shell the
  // failure belongs to the strip's row, not to a full-screen error.
  if (!canHost()) return 'this page is not the client';
  const mine = ++generation;
  const here = drawsSurface();
  if (here) {
    surface.phase = 'opening';
    surface.profile = profile;
    surface.reason = null;
  }
  const why = await showBrowser(profile, seat);
  // **The phone's takeover is the native one**: its shell raised it, and the
  // page's components are the desktop's.
  if (!here) return why;
  if (mine !== generation) {
    // A stale open that landed after a close still has a surface up in the
    // shell; it is taken down rather than left drawing under a closed store.
    if (why === null && surface.phase === 'closed') void hideBrowser(profile);
    return why;
  }
  if (why !== null) {
    surface.phase = 'failed';
    surface.reason = why;
    return why;
  }
  surface.phase = 'live';
  installSurfaceFailure();
  return null;
}

/**
 * Take the surface away; the browser keeps running. `profile` names the one
 * to lower - a row knows its own name even when this page's store was reset
 * by a reload while the shell's surface stayed up. Answers why it could not,
 * or `null` when it is closed.
 */
export async function closeSurface(
  profile: string | null = surface.profile,
): Promise<string | null> {
  ++generation;
  const answer = await hideBrowser(profile);
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
  } else {
    surface.phase = 'live';
  }
  return null;
}

/** Whether the shell-failure listener is installed; one per page. */
let watching = false;

/**
 * The shell's own failure state: a host that stopped answering or a surface
 * that could not be placed arrives as an event, and the takeover draws the
 * sentence with its bar still up - the hand-off stays parked.
 */
export function installSurfaceFailure(): void {
  if (watching || !canHost()) return;
  watching = true;
  void import('@tauri-apps/api/event')
    .then(async ({ listen }) => {
      // **The keyboard's way out.** The overlay holds first responder while
      // the surface is up; Cmd+W arrives as this event, and the page lowers
      // the surface the same way back-to-chat does - the hand-off stays
      // parked for the bar's Done or Not now.
      await listen('browser_surface_lower', () => {
        void closeSurface();
      });
      await listen<string>('browser_surface_failed', (event) => {
        const why =
          typeof event.payload === 'string' && event.payload !== ''
            ? event.payload
            : 'the browser stopped answering';
        surface.phase = 'failed';
        surface.reason = why;
        // The shell's surface is gone with the host; the page's keeps the
        // sentence up until the person answers the hand-off.
        void hideBrowser(surface.profile);
      });
    })
    .catch(() => {
      // No event API (a plain page, a test harness): the failure state is
      // the shell's, and there is nothing here to listen with.
      watching = false;
    });
}

/** The takeover's page area, as the shell places the native layer: CSS pixels
 *  from the viewport's top-left, and the device pixel ratio drawn at. */
export function setRegion(
  rect: { x: number; y: number; width: number; height: number },
  scale: number,
): void {
  void reportSurfaceRect(rect, scale);
}

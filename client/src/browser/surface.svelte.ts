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
import { hideBrowser, reportSurfaceRect, showBrowser } from './host';

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
  const mine = ++generation;
  surface.phase = 'opening';
  surface.profile = profile;
  surface.reason = null;
  const why = await showBrowser(profile, seat);
  if (mine !== generation) return why;
  if (why !== null) {
    surface.phase = 'failed';
    surface.reason = why;
    return why;
  }
  surface.phase = 'live';
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
  const why = await hideBrowser(profile);
  if (why !== null) {
    surface.phase = 'failed';
    surface.reason = why;
    return why;
  }
  surface.phase = 'closed';
  surface.profile = null;
  surface.reason = null;
  return null;
}

/** The page's column, as the shell places the native layer: CSS pixels from
 *  the viewport's top-left, and the device pixel ratio drawn at. */
export function setRegion(
  rect: { x: number; y: number; width: number; height: number },
  scale: number,
): void {
  void reportSurfaceRect(rect, scale);
}

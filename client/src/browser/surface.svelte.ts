/**
 * The browser surface: which profile's page the conversation column is
 * drawing, and every state between the person's Open and the surface going
 * away.
 *
 * **One surface at a time, and the host outlives it.** Closing the surface
 * is not closing the browser: the sessions keep driving the same page, and
 * the next Open shows it exactly as they left it.
 *
 * A superseded open never lands: the generation is read after every await,
 * so a second Open (or a close) while the first is still starting leaves the
 * store on the LATEST request, not the slowest one.
 */

import type { SessionSlot } from '../wire/types';
import { hideBrowser, reportSurfaceRect, showBrowser } from './host';

export type SurfacePhase = 'closed' | 'opening' | 'live' | 'failed';

export const surface = $state({
  phase: 'closed' as SurfacePhase,
  /** The name the person opened, `null` for the shared browser. */
  profile: null as string | null,
  /** The shell's sentence when the surface could not open. */
  reason: null as string | null,
});

/** The request that is current; a result from an older one is dropped. */
let generation = 0;

/**
 * Open the surface over the column for `profile` (`null` is shared). Answers
 * why it could not open, or `null` when it did.
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

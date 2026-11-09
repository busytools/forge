/**
 * Whether the page is being watched: visible and with focus.
 *
 * **Two signals, because a locked screen is a lost focus and not a hidden
 * document**: the desktop window loses key status when the screen locks and
 * the page hears it as `blur`, while a background tab or a minimized window
 * reports `visibilitychange`. **A blur is reported as itself** rather than
 * re-read, because `hasFocus()` has not settled when its handler runs.
 */

import { cannotPaint } from './paint';

/** Whether the page is being watched right now: visible and with focus. */
export function pageWatched(): boolean {
  return typeof document === 'undefined' || (!cannotPaint() && document.hasFocus());
}

/**
 * Reports whether the page is being watched as that changes; returns its own
 * disconnect, or null where there is no document.
 */
export function observeAttention(cb: (watching: boolean) => void): (() => void) | null {
  if (typeof document === 'undefined') return null;
  const lost = (): void => cb(false);
  const gained = (): void => cb(!cannotPaint());
  const visibility = (): void => cb(!cannotPaint() && document.hasFocus());
  document.addEventListener('visibilitychange', visibility);
  window.addEventListener('focus', gained);
  window.addEventListener('blur', lost);
  // A bfcache restore comes back visible with neither a focus nor a
  // visibility event on some engines, so the page's own restore event arms it.
  window.addEventListener('pageshow', gained);
  return () => {
    document.removeEventListener('visibilitychange', visibility);
    window.removeEventListener('focus', gained);
    window.removeEventListener('blur', lost);
    window.removeEventListener('pageshow', gained);
  };
}

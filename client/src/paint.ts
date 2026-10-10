/**
 * Whether a store's write has a paint to wait for.
 *
 * **A page that cannot paint has no paint to wait for.** Waiting for a frame
 * is what turns a burst of updates into one draw of the latest, and that is
 * worth having on a page the display is refreshing. A hidden page is not
 * being refreshed at all - WebKit stops producing frames entirely when its
 * window is not visible, and `document.hidden` is true in exactly that state
 * (measured 2026-10-10: an offscreen WKWebView reports hidden with a frame
 * counter at 0) - so a write that waited there would wait for something the
 * page cannot produce, and the column would stay stale until it was visible
 * again. It goes out at once instead, which costs nothing while nothing is
 * painting and leaves the DOM current for the moment it paints again.
 */

/**
 * Whether the page cannot paint right now.
 *
 * **A page that is not there cannot paint either.** The conversation store is
 * driven with no document at all - `chat/one-row-per-running-turn.test.ts`,
 * which runs in the node environment - so the question is only asked where
 * there is a page to ask.
 */
export function cannotPaint(): boolean {
  return typeof document !== 'undefined' && document.hidden;
}

/**
 * When a store's write waits for a painted frame, and when it does not.
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
 * Run `flush` on the next painted frame, or at once when the page is hidden.
 *
 * Answers the frame the flush is owed on, or `null` when it has already run -
 * so a caller arms its own deadline only when one is owed.
 */
export function whenPainted(flush: () => void): number | null {
  if (document.hidden) {
    flush();
    return null;
  }
  return requestAnimationFrame(flush);
}

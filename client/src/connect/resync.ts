/**
 * The return probe: the one ask a page still owes when the person comes back.
 *
 * **A socket can outlive the screen going off.** The transport's own liveness
 * probe catches a link that died (a half-open socket answers nothing), but a
 * link that is genuinely still open carries no news of the hours it slept
 * through - so a return probes once, and a dead socket is caught rather than
 * waiting out its quiet spell.
 *
 * **A return reads nothing.** The subscriptions stayed open while the page was
 * away, so home, the board and every held seat arrived as patches and were
 * applied as they landed; a webview that was throttled applies what queued the
 * moment it paints again. A socket that actually dropped re-subscribes every
 * held subject on reconnect, and that answer is the whole read - so paying a
 * read for a return would buy the same bytes twice.
 */

import { observeForeground } from '../foreground';
import type { Connection } from '../socket';

/**
 * How long a hide must last before a return re-reads.
 *
 * **The same window is the dedupe**: two triggers naming one return - a
 * mobile `Resumed` beside the page's own `visibilitychange` - must not buy
 * two full reads, and anything inside this window is one return.
 */
export const RETURN_HIDE_MS = 2_000;

/**
 * Probe the socket a return is owed, once.
 *
 * A socket that died while the page slept is caught here rather than up to a
 * quiet spell later; the read a dead one costs is the reconnect's own.
 */
export function resync(connection: Connection): void {
  if (connection.status() !== 'open') return;
  connection.probe();
}

/**
 * Watch for the person coming back, and probe once each time they do.
 *
 * Answers a function that stops watching. The shell's event is a promise (the
 * Tauri API is imported lazily so the web build never loads it), so a stop
 * that arrives before the listener resolves takes it back on resolution.
 */
export function watchReturn(connection: Connection): () => void {
  let stopped = false;
  let hiddenAt = typeof document !== 'undefined' && document.hidden ? Date.now() : null;
  let lastReadAt = 0;
  const stops: (() => void)[] = [];

  const freshen = (): void => {
    if (stopped) return;
    const now = Date.now();
    // **A shell resume spends the hide**: a bare visible signal after it is
    // the same return, however long the dedupe window has been.
    hiddenAt = null;
    if (now - lastReadAt < RETURN_HIDE_MS) return;
    lastReadAt = now;
    resync(connection);
  };

  const visible = (): void => {
    if (document.hidden) {
      hiddenAt = Date.now();
      return;
    }
    const since = hiddenAt;
    hiddenAt = null;
    if (since === null || Date.now() - since < RETURN_HIDE_MS) return;
    freshen();
  };

  if (typeof document !== 'undefined') {
    document.addEventListener('visibilitychange', visible);
  }

  // **A mobile resume is its own signal.** Android's window focus is
  // unreliable (`asks.rs`), so the shell emits a resume the page listens for
  // - and a page reachable only from a shell must not load the Tauri API
  // where there is none.
  const stopForeground = observeForeground('return probe', freshen);
  if (stopForeground !== null) stops.push(stopForeground);

  return () => {
    stopped = true;
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', visible);
    }
    for (const stop of stops) stop();
    stops.length = 0;
  };
}

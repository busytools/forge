/**
 * The return re-read: what a page asks for again when the person comes back.
 *
 * **A socket can outlive the screen going off.** The transport's own liveness
 * probe catches a link that died (a half-open socket answers nothing), but a
 * link that is genuinely still open carries no news of the hours it slept
 * through: the records the page holds are as old as the hide, and a seat's
 * conversation does not refresh itself while nobody draws it. So a return
 * asks once - home, board and the shown seat - and the probe goes
 * first so a dead socket is caught by its deadline rather than up to a quiet
 * spell later.
 *
 * **Only a return that left the screen counts.** A desktop focus change and a
 * glance at another window leave the document visible and re-read nothing;
 * the seat read is 2.4-3.7 MB (`read-bound.ts`), and paying that for an
 * alt-tab would cost more than the staleness it fixes.
 */

import { canHost } from '../browser/host';
import type { Subject } from '../protocol';
import { rereadSeat } from '../session/live';
import { report, type Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * How long a hide must last before a return re-reads.
 *
 * **The same window is the dedupe**: two triggers naming one return - a
 * mobile `Resumed` beside the page's own `visibilitychange` - must not buy
 * two full reads, and anything inside this window is one return.
 */
export const RETURN_HIDE_MS = 2_000;

/**
 * The subjects a return freshens, when this connection holds them.
 *
 * **`usage` is not here**, though the server serves it: no page subscribes to
 * it - the token and cost pool rides the home snapshot (`Subject::Usage`'s own
 * doc says a subscription is one scan and hears nothing after it), so a
 * refresh for it would be an ask nothing holds.
 */
const RETURNED: readonly Subject[] = ['home', 'board'];

/**
 * Re-read what a return makes stale, once.
 *
 * The probe first: a socket that died while the page slept would otherwise
 * spend its read on a frame that never arrives.
 */
export function resync(connection: Connection, shown: SessionSlot | null): void {
  if (connection.status() !== 'open') return;
  connection.probe();
  for (const what of RETURNED) {
    if (connection.store(what) !== undefined) connection.refresh(what);
  }
  if (shown !== null) rereadSeat(connection, shown);
}

/**
 * Watch for the person coming back, and freshen once each time they do.
 *
 * `shown` is read when the trigger fires rather than when this is installed:
 * the route moves under the watcher, and a captured slot would re-read a seat
 * the page left.
 *
 * Answers a function that stops watching. The shell's event is a promise (the
 * Tauri API is imported lazily so the web build never loads it), so a stop
 * that arrives before the listener resolves takes it back on resolution.
 */
export function watchReturn(connection: Connection, shown: () => SessionSlot | null): () => void {
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
    resync(connection, shown());
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
  if (canHost()) {
    void import('@tauri-apps/api/event')
      .then(({ listen }) => listen('client://foreground', () => freshen()))
      .then((stop) => {
        if (stopped) stop();
        else stops.push(stop);
      })
      .catch((why: unknown) => {
        report('the shell foreground listener could not be installed', why);
      });
  }

  return () => {
    stopped = true;
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', visible);
    }
    for (const stop of stops) stop();
    stops.length = 0;
  };
}

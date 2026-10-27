/**
 * The browser through the page's own socket: the only link a client with no
 * shell has.
 *
 * **The wire already carries all of it.** An attach is a `browser_view` whose
 * reply names the profile and tab it took; the tabs are a `browser_tabs` whose
 * reply is the list, then a push per change; the gestures are their own
 * messages; and the frames ride the socket's binary channel. What this file
 * adds is only the page's side: correlate an ask with its reply, keep the
 * newest frame for a canvas that mounts late, and hand the pushed messages to
 * the surface's own handlers in the shape they already read.
 */

import type { BrowserTabList } from '../protocol';
import type { BrowserFrame, BrowserWire, Connection } from '../socket';
import { tabListOf, whyText, type HideAnswer, type OpenAnswer } from './host';
import type { BrowserLink } from './link';

function wireLink(connection: Connection & BrowserWire): BrowserLink {
  /**
   * The newest frame the socket relayed.
   *
   * **An attach's key frame can land before the canvas mounts**, and a still
   * page never repaints; the shell keeps its newest one for the same reason.
   * The server relays only the viewed tab's frames to a viewer, so the newest
   * is this surface's.
   */
  let newest: BrowserFrame | null = null;
  connection.onBrowserFrame((frame) => {
    newest = frame;
  });

  /** The size this page last asked the server for, so a repeat asks nothing. */
  let asked: { width: number; height: number; scale: number } | null = null;

  return {
    attachable: true,
    drawsSurface: true,
    paintsCanvas: true,
    /** **A page is the only client behind its own view**, so it sizes it. */
    sizesAsWatcher: true,
    /** **The attach lived on the socket that dropped**, so the page re-asks. */
    reattachesOnReconnect: true,
    /** A tab's Back would leave the seat; it lowers the takeover instead. */
    usesPageHistory: true,

    async show(profile, seat, tab): Promise<OpenAnswer> {
      if (seat === null) return 'this page has no seat to attach the browser to';
      void profile;
      // **An attach re-asks the size.** Another client may have moved the
      // profile's surface while this page was away, so the cache cannot say
      // what the page is laid out at until this page asks again.
      asked = null;
      try {
        return await connection.browserView(seat, tab);
      } catch (why) {
        return whyText(why);
      }
    },

    hide(profile, seat): Promise<HideAnswer> {
      void profile;
      void seat;
      return Promise.resolve(connection.browserDetach() ? true : 'the connection is not open');
    },

    /** **The page is its own surface**: its box is what it draws in. */
    reportRect: () => Promise.resolve(),

    resize(width, height, scale): Promise<void> {
      // **A size the page already has is not a request.** A re-measure that
      // lands on the same box must not force the host through another layout.
      const last = asked;
      if (last !== null && last.width === width && last.height === height && last.scale === scale) {
        return Promise.resolve();
      }
      if (connection.browserResize(width, height, scale)) asked = { width, height, scale };
      return Promise.resolve();
    },

    navigate(direction): Promise<void> {
      connection.browserNav(direction === 'back');
      return Promise.resolve();
    },

    takeControl(take): Promise<void> {
      // **Taking the role re-lays the page out.** Another client may have
      // sized it while this page watched, and the server refuses a watcher's
      // resize under a held role - so the cache cannot answer for this one.
      if (take) asked = null;
      connection.browserControl(take);
      return Promise.resolve();
    },

    keptFrame(): Promise<BrowserFrame | null> {
      return Promise.resolve(newest);
    },

    async listTabs(seat): Promise<BrowserTabList> {
      // **A tab list names a seat**: the ask carries it, and a page with none
      // has no profile to list - refused here rather than as a malformed frame.
      if (seat === null) throw new Error('this page has no seat to list the browser for');
      return tabListOf(await connection.browserTabs(seat));
    },

    onTabs(fn): Promise<() => void> {
      return Promise.resolve(
        connection.onMessage((message) => {
          if (message.kind !== 'browser_tabs') return;
          try {
            fn(tabListOf(message));
          } catch (why) {
            console.warn(`a pushed tab list could not be read: ${whyText(why)}`);
          }
        }),
      );
    },

    onConnection(fn): Promise<() => void> {
      return Promise.resolve(
        connection.onStatus((status) => {
          if (status === 'open') fn();
        }),
      );
    },

    onView(fn): Promise<() => void> {
      return Promise.resolve(
        connection.onMessage((message) => {
          if (message.kind === 'browser_view') fn(message);
        }),
      );
    },

    onNav(fn): Promise<() => void> {
      return Promise.resolve(
        connection.onMessage((message) => {
          if (message.kind === 'browser_nav_state') fn(message);
        }),
      );
    },

    onFailed(fn): Promise<() => void> {
      return Promise.resolve(
        connection.onMessage((message) => {
          if (message.kind === 'browser_surface_failed') fn(message);
        }),
      );
    },
  };
}

export { wireLink };

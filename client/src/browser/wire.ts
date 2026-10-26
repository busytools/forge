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
import type { SessionSlot } from '../wire/types';
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

  return {
    attachable: true,
    drawsSurface: true,
    paintsCanvas: true,

    async show(profile, seat, tab): Promise<OpenAnswer> {
      if (seat === null) return 'this page has no seat to attach the browser to';
      void profile;
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
      connection.browserResize(width, height, scale);
      return Promise.resolve();
    },

    navigate(direction): Promise<void> {
      connection.browserNav(direction === 'back');
      return Promise.resolve();
    },

    takeControl(take): Promise<void> {
      connection.browserControl(take);
      return Promise.resolve();
    },

    keptFrame(): Promise<BrowserFrame | null> {
      return Promise.resolve(newest);
    },

    async listTabs(seat): Promise<BrowserTabList> {
      return tabListOf(await connection.browserTabs(seat as SessionSlot));
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

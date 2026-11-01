/**
 * The browser through the client's shell: the native overlay the Rust half
 * paints, the window's takeover, and the events it pushes.
 *
 * **The browser is not this process's.** A surface attaches to a profile over
 * the shell's socket; the shell paints the server's frames in a native layer
 * and sends the person's gestures back. What is left here is the page's side
 * of that: open, lower, measure, take control, move through history.
 */

import type { InvokeArgs } from '@tauri-apps/api/core';

import type { BrowserTabList } from '../protocol';
import type { BrowserFrame } from '../socket';
import { bytesOf, tabListOf, whyText, type HideAnswer, type OpenAnswer } from './host';
import type { BrowserLink } from './link';

/** The shell's `invoke`, loaded lazily so this module reads outside Tauri. */
async function invokeShell<T>(command: string, args?: InvokeArgs): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<T>(command, args);
}

/** The attach the shell answered, or `null` for a shape this page cannot read. */
function attachedFrom(value: unknown): { profile: string; tab: number } | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  if (typeof held['profile'] !== 'string' || held['profile'] === '') return null;
  if (typeof held['tab'] !== 'number' || !Number.isInteger(held['tab'])) return null;
  return { profile: held['profile'], tab: held['tab'] };
}

function shellLink(): BrowserLink {
  /** **The phone keeps the picture in its page**: its shell has no native
   *  overlay, so the frames the Rust half relays land in a canvas there, and
   *  the document owns the keyboard either way. */
  const android = typeof navigator !== 'undefined' && /Android/i.test(navigator.userAgent);
  return {
    attachable: true,
    drawsSurface: true,
    paintsCanvas: android,
    /** The shell's watcher window never moves the page under the driver. */
    sizesAsWatcher: false,
    /** The bridge re-attaches the viewer on its own socket. */
    reattachesOnReconnect: false,
    /** An overlay view holds AppKit's first responder on the desktop. */
    pageHoldsKeyboard: !android,
    /** The shell's own hardware Back lowers the takeover. */
    usesPageHistory: false,

    async show(profile, seat, tab): Promise<OpenAnswer> {
      try {
        const answer: unknown = await invokeShell('client_browser_view', { seat, tab });
        await invokeShell('browser_takeover', { on: true });
        void profile;
        const attached = attachedFrom(answer);
        if (attached === null) {
          return 'the shell attached without naming the profile and tab';
        }
        return attached;
      } catch (why) {
        return whyText(why);
      }
    },

    async hide(profile, seat): Promise<HideAnswer> {
      try {
        await invokeShell('browser_takeover', { on: false });
        void profile;
        void seat;
        return true;
      } catch (why) {
        return whyText(why);
      }
    },

    async reportRect(rect, viewport, scale): Promise<void> {
      await invokeShell('browser_surface_rect', { rect: { ...rect, scale }, viewport });
    },

    async resize(width, height, scale): Promise<void> {
      try {
        await invokeShell('client_browser_resize', { width, height, scale });
      } catch (why) {
        console.warn('the page was not laid out to this surface', why);
      }
    },

    async navigate(direction): Promise<void> {
      try {
        await invokeShell('client_browser_nav', { back: direction === 'back' });
      } catch (why) {
        console.warn('the page did not move through its history', why);
      }
    },

    async takeControl(take): Promise<void> {
      await invokeShell('client_browser_control', { take });
    },

    /** The pane chord's way back: the overlay takes first responder again. */
    async focusPage(): Promise<void> {
      try {
        await invokeShell('browser_surface_focus', { holder: 'page' });
      } catch (why) {
        console.warn('the keyboard did not go back to the page', why);
      }
    },

    /** Focus landing in the bar: the webview takes it from the picture. */
    async focusBar(): Promise<void> {
      try {
        await invokeShell('browser_surface_focus', { holder: 'bar' });
      } catch (why) {
        console.warn('the keyboard did not come into the bar', why);
      }
    },

    async keptFrame(): Promise<BrowserFrame | null> {
      try {
        const raw = await invokeShell<Record<string, unknown> | null>('browser_kept_frame');
        if (raw === null || typeof raw !== 'object') return null;
        const { profile, tab, seq, width, height, page_width, page_height, bytes } = raw;
        if (
          typeof profile !== 'number' ||
          typeof tab !== 'number' ||
          typeof width !== 'number' ||
          typeof height !== 'number' ||
          typeof page_width !== 'number' ||
          typeof page_height !== 'number' ||
          typeof bytes !== 'string'
        ) {
          return null;
        }
        return {
          profile,
          tab,
          seq: typeof seq === 'number' ? seq : 0,
          width,
          height,
          page_width,
          page_height,
          key: raw.key === true,
          bytes: bytesOf(bytes),
        };
      } catch {
        return null;
      }
    },

    async listTabs(seat): Promise<BrowserTabList> {
      const body = await invokeShell<Record<string, unknown>>('client_browser_tabs', { seat });
      return tabListOf(body['tabs']);
    },

    async onTabs(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<unknown>('browser_tabs', (event) => {
        try {
          fn(tabListOf(event.payload));
        } catch (why) {
          console.warn('a pushed tab list could not be read', why);
        }
      });
    },

    async onConnection(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen('client://connection', () => fn());
    },

    async onView(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<Record<string, unknown>>('browser_view', (event) => fn(event.payload));
    },

    async onNav(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<Record<string, unknown>>('browser_nav_state', (event) =>
        fn(event.payload),
      );
    },

    async onFailed(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<unknown>('browser_surface_failed', (event) => fn(event.payload));
    },

    async onLower(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen('browser_surface_lower', () => fn());
    },

    async onRemeasure(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen('browser_surface_remeasure', () => fn());
    },

    async onModifier(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<unknown>('browser_modifier', (event) => {
        const held = event.payload;
        if (typeof held !== 'object' || held === null) return;
        const { code, down } = held as Record<string, unknown>;
        if (typeof code !== 'number' || typeof down !== 'boolean') return;
        fn({ code, down });
      });
    },

    async onModifierChord(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen('browser_modifier_chord', () => fn());
    },

    async onKeyboard(fn): Promise<() => void> {
      const { listen } = await import('@tauri-apps/api/event');
      return await listen<unknown>('browser_keyboard', (event) => {
        const held = event.payload;
        if (typeof held !== 'object' || held === null) return;
        const holder = (held as Record<string, unknown>)['holder'];
        if (holder !== 'bar' && holder !== 'page') return;
        fn(holder);
      });
    },
  };
}

export { shellLink };

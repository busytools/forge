/**
 * The shell as the browser's viewer: the page's window onto a browser the
 * SERVER owns.
 *
 * **The browser is not this process's any more.** A surface attaches to a
 * profile over the socket; the shell paints the server's frames in a native
 * layer and sends the person's gestures back. What is left here is the page's
 * side of that: open, lower, measure, take control, move through history.
 *
 * **A page outside the shell draws nothing.** The dev server in a plain
 * browser has no shell to paint into, so the surface code answers its own
 * refusal rather than invoking a command that is not there.
 */

import type { InvokeArgs } from '@tauri-apps/api/core';

import type { BrowserTabList } from '../protocol';
import type { BrowserFrame } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * Whether this page runs inside the client shell: the Tauri marker is the one
 * gate. The web build draws its browser in the page, not here.
 */
export function canHost(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/**
 * Whether this page draws the takeover: the shell's bar and states, over a
 * native layer on the desktop and over the page's own canvas on the phone.
 */
export function drawsSurface(): boolean {
  return canHost();
}

/**
 * Whether this page paints the frames itself, on a canvas. **The phone
 * does**: its shell has no native overlay, so the frames the Rust half
 * relays arrive in the page (the desktop's overlay draws them instead).
 */
export function drawsCanvas(): boolean {
  return typeof navigator !== 'undefined' && canHost() && /Android/i.test(navigator.userAgent);
}

/**
 * The reason a rejected command carried, as a sentence.
 *
 * Exported for the strip's own reads: a failed read has the shell's own words
 * in its rejection, and a caller that drew anything else would be inventing a
 * reason.
 */
export function whyText(why: unknown): string {
  if (typeof why === 'string' && why !== '') return why;
  return `the client's browser viewer failed: ${String(why)}`;
}

/** The bytes a base64 string carries. */
export function bytesOf(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let at = 0; at < binary.length; at += 1) {
    bytes[at] = binary.charCodeAt(at);
  }
  return bytes;
}

/** The shell's `invoke`, loaded lazily so this module reads outside Tauri. */
async function invokeShell<T>(command: string, args?: InvokeArgs): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<T>(command, args);
}

/**
 * Attach to a seat's browser tab and put the takeover up: what a hand-off's
 * Open and the strip's Show ask for.
 *
 * `tab` is the tab to view, and `null` is the seat's current tab (else the
 * profile's first), which is what the profile's own row opens. **Answers the
 * profile and tab the server actually attached**, or the shell's reason it
 * could not: the resolved pair is what the page lowers and filters frames by,
 * and the server's own sentence is carried through, so a dock that claimed
 * the browser was up over an open that never happened cannot have the person
 * press Done and tell the session they acted.
 */
export type OpenAnswer = { profile: string; tab: number } | string;

/** The attach the shell answered, or `null` for a shape this page cannot read. */
function attachedFrom(value: unknown): { profile: string; tab: number } | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  if (typeof held['profile'] !== 'string' || held['profile'] === '') return null;
  if (typeof held['tab'] !== 'number' || !Number.isInteger(held['tab'])) return null;
  return { profile: held['profile'], tab: held['tab'] };
}

export async function showBrowser(
  profile: string | null,
  seat: SessionSlot | null,
  tab: number | null,
  epoch: number,
): Promise<OpenAnswer> {
  if (!canHost()) return 'this page is not the client';
  try {
    const answer: unknown = await invokeShell('client_browser_view', { seat, tab });
    await invokeShell('browser_takeover', { on: true });
    void epoch;
    void profile;
    const attached = attachedFrom(answer);
    if (attached === null) {
      return 'the shell attached without naming the profile and tab';
    }
    return attached;
  } catch (why) {
    return whyText(why);
  }
}

/**
 * Take the takeover down: the server's browser keeps running, with its tabs
 * and its page, and the next agent call drives that same browser. **`seat` is
 * the asking hand-off's when `profile` is null**: the raise resolved it to
 * that seat's own profile, and Done must lower the same browser.
 */
export async function hideBrowser(
  profile: string | null = null,
  seat: SessionSlot | null = null,
): Promise<HideAnswer> {
  if (!canHost()) return 'this page is not the client';
  try {
    await invokeShell('browser_takeover', { on: false });
    void profile;
    void seat;
    return { lowered: true };
  } catch (why) {
    return whyText(why);
  }
}

/**
 * What a lower answered: whether it took the page the person sees down.
 */
export type HideAnswer = { lowered: boolean } | string;

/**
 * Where the takeover's page area is, for the shell to place the native layer:
 * CSS pixels from the viewport's top-left and the device pixel ratio. The
 * page is the measurer, so the bar's own height is in the rect rather than a
 * number the shell would have to guess.
 *
 * **The page's viewport goes with it.** A titled window's page is shorter
 * than the view the shell lays the picture in (the chrome above it), and the
 * shell needs that inset to keep the top of the picture off the bar.
 */
export async function reportSurfaceRect(
  rect: { x: number; y: number; width: number; height: number },
  viewport: { width: number; height: number },
  scale: number,
  epoch: number,
): Promise<void> {
  if (!canHost()) return;
  await invokeShell('browser_surface_rect', { rect: { ...rect, scale }, viewport, epoch });
}

/**
 * Lay the page out to this surface: device pixels and the ratio they draw at.
 *
 * **Only the controller's size is honoured**, so this goes out while this
 * page holds the role and at the moment it takes it.
 */
export async function resizeBrowser(width: number, height: number, scale: number): Promise<void> {
  if (!canHost()) return;
  try {
    await invokeShell('client_browser_resize', { width, height, scale });
  } catch (why) {
    console.warn('the page was not laid out to this surface', why);
  }
}

/**
 * Move the takeover's page through its own history: the bar's arrows.
 *
 * **The arrows' state is not read back from here** - the server pushes it as
 * `browser_nav_state` - so a refused move is logged rather than drawn.
 */
export async function navigateBrowser(direction: 'back' | 'forward'): Promise<void> {
  if (!canHost()) return;
  try {
    await invokeShell('client_browser_nav', { back: direction === 'back' });
  } catch (why) {
    console.warn('the page did not move through its history', why);
  }
}

/**
 * Take the controller role for the surface, or give it back. **Exactly one
 * viewer holds it**: a take moves the role, and the server tells every viewer
 * who holds it now.
 */
export async function takeControl(take: boolean): Promise<void> {
  if (!canHost()) return;
  await invokeShell('client_browser_control', { take });
}

/**
 * The newest relayed frame, for a canvas that mounts after it arrived.
 *
 * **An attach's key frame can land before the surface subscribes**, and a
 * still page never repaints; the shell keeps the newest one for this.
 */
export async function keptFrame(): Promise<BrowserFrame | null> {
  if (!canHost()) return null;
  try {
    const raw = await invokeShell<Record<string, unknown> | null>('browser_kept_frame');
    if (raw === null || typeof raw !== 'object') return null;
    const { profile, tab, seq, width, height, bytes } = raw;
    if (
      typeof profile !== 'number' ||
      typeof tab !== 'number' ||
      typeof width !== 'number' ||
      typeof height !== 'number' ||
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
      key: raw.key === true,
      bytes: bytesOf(bytes),
    };
  } catch {
    return null;
  }
}

/**
 * One tab as the strip draws it.
 *
 * `state` is the server's own word, and `stateText` is how a row spells it -
 * a state this page has never seen is drawn as itself rather than as a
 * neighbour it does not mean.
 */
export interface TabRow {
  number: number;
  title: string;
  url: string;
  /** The owning session as `org/project/label`. */
  owner: string;
  /** The server's own word, kept whole for a state this page has not seen. */
  state: string;
  stateText: string;
}

/** How one tab state reads in a row. */
const STATE_TEXT: Record<string, string> = {
  in_use: 'in use',
  idle: 'idle',
  held: 'held',
};

/**
 * The strip's rows, from one tab list.
 *
 * **A row that names no owner is refused, not drawn**: every tab belongs to a
 * session, and a row with none cannot say whose calls drive it - so the
 * strip's read fails with the reason rather than inventing an owner.
 */
export function tabRows(list: BrowserTabList): TabRow[] {
  return list.rows.map((row) => {
    const owner = slotLabel(row.owner);
    if (owner === '') {
      throw new Error(`tab ${row.number} names no owner`);
    }
    const state = row.state;
    return {
      number: row.number,
      // **A tab with no title reads as its address**: `about:blank` is the
      // honest name for a blank page, where an empty cell says nothing.
      title: row.title !== '' ? row.title : row.url !== '' ? row.url : `tab ${row.number}`,
      url: row.url,
      owner,
      state,
      stateText: STATE_TEXT[state] ?? String(state),
    };
  });
}

/** The tabs of this seat's profile, as the strip and the bar draw them. */
export async function listTabs(seat: SessionSlot | null = null): Promise<BrowserTabList> {
  if (!canHost()) throw new Error('this page is not the client');
  const body = await invokeShell<Record<string, unknown>>('client_browser_tabs', { seat });
  return tabListOf(body['tabs']);
}

/** One tab list from the wire, narrowed once here; a bad envelope throws. */
export function tabListOf(value: unknown): BrowserTabList {
  if (typeof value !== 'object' || value === null) {
    throw new Error('the server sent no tab list');
  }
  const held = value as Record<string, unknown>;
  if (typeof held['profile'] !== 'string' || typeof held['id'] !== 'number') {
    throw new Error('the tab list names no profile');
  }
  const closing = held['closing_at_ms'];
  if (closing !== null && closing !== undefined && typeof closing !== 'number') {
    throw new Error('the tab list carries no readable close time');
  }
  return {
    profile: held['profile'],
    id: held['id'],
    up: held['up'] === true,
    connected: held['connected'] === true,
    closing_at_ms: typeof closing === 'number' ? closing : null,
    // The rows are validated where they are drawn (`tabRows`), so a row this
    // page cannot read is refused by name rather than dropped here.
    rows: Array.isArray(held['rows']) ? (held['rows'] as BrowserTabList['rows']) : [],
  };
}

/**
 * Follow the tab list the shell pushes, as the strip and the bar do.
 *
 * The ask (`listTabs`) subscribes the connection; this is the same stream
 * after it. Answers a function that stops listening.
 */
export async function onTabs(fn: (list: BrowserTabList) => void): Promise<() => void> {
  if (!canHost()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  return await listen<unknown>('browser_tabs', (event) => {
    try {
      fn(tabListOf(event.payload));
    } catch (why) {
      console.warn('a pushed tab list could not be read', why);
    }
  });
}

/** A slot as the strip's own line reads it, or `''` when none came. */
function slotLabel(owner: unknown): string {
  if (typeof owner !== 'object' || owner === null) return '';
  const slot = owner as Record<string, unknown>;
  const parts = [slot.org, slot.project, slot.label];
  return parts.every((part) => typeof part === 'string') ? parts.join('/') : '';
}

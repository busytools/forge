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
 * Attach to a profile's browser and put the takeover up: what a hand-off's
 * Open asks for. **`profile` names which one**: a name is that profile's own,
 * `null` is the asking seat's own when a seat rides along (or the shared
 * browser for the person's own door), and `@shared` is the shared browser by
 * name - the CAPTCHA in the right session, not a lookalike in the wrong one.
 *
 * **Answers the profile it raised, or the shell's reason it could not.** The
 * raised name is what the page lowers on Done; the server's own sentence is
 * carried through, so a dock that claimed the browser was up over an open
 * that never happened cannot have the person press Done and tell the session
 * they acted.
 */
export type OpenAnswer = { profile: string | null } | string;

export async function showBrowser(
  profile: string | null,
  seat: SessionSlot | null,
  epoch: number,
): Promise<OpenAnswer> {
  if (!canHost()) return 'this page is not the client';
  try {
    // **The profile the server actually attached**: a raise with no profile
    // is resolved to the asking seat's own, and Done must lower that very
    // browser rather than a name this page guessed.
    //
    // **An answer that names nothing keeps the name the person opened.** A
    // shell from before the attach answered with its profile returns null
    // here, and taking that null would leave the bar unable to say which
    // browser it is over.
    const attached: unknown = await invokeShell('client_browser_view', { seat, profile });
    await invokeShell('browser_takeover', { on: true });
    void epoch;
    const named = typeof attached === 'string' && attached !== '' ? attached : profile;
    return { profile: named };
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
    const { profile, seq, width, height, bytes } = raw;
    if (
      typeof profile !== 'number' ||
      typeof width !== 'number' ||
      typeof height !== 'number' ||
      typeof bytes !== 'string'
    ) {
      return null;
    }
    return {
      profile,
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
 * The profiles the server holds, for the client's own strip.
 *
 * **The server owns the profiles**, so the rows are its answer, narrowed
 * here once: a row this page cannot read is drawn as itself (`unreadable`),
 * never dropped.
 */
export async function listProfiles(seat: SessionSlot | null = null): Promise<ProfileRow[]> {
  if (!canHost()) return [];
  const body = await invokeShell<Record<string, unknown>>('client_browser_profiles', { seat });
  const rows = Array.isArray(body.rows) ? body.rows : [];
  // **A row this page cannot read is drawn as itself, never dropped**
  // (rule 25): filtering it also made the count and the empty line report a
  // state the server never sent.
  return rows.map((value, at) => profileRow(value, at));
}

/** One profile, as the client's own browser strip draws it. */
export interface ProfileRow {
  name: string;
  /** The slot of the session that opened it, as `org/project/label`. */
  owner: string;
  /** Whether its browser host is up. */
  up: boolean;
  /** Whether a client has it open as the takeover right now. */
  windowed: boolean;
  /** Whether a browser call is running on it now. */
  busy: boolean;
  /** Whether it is known only on disk: no browser is open for it. */
  closed: boolean;
  /** The server sent a row this page cannot read; it is drawn, not acted on. */
  unreadable?: boolean;
}

/** One row from the wire, drawn as itself when it cannot be read. */
function profileRow(value: unknown, at: number): ProfileRow {
  if (typeof value !== 'object' || value === null) {
    return unreadable(at);
  }
  const row = value as Record<string, unknown>;
  if (typeof row.name !== 'string' || row.name === '') {
    return unreadable(at);
  }
  return {
    name: row.name,
    owner: slotLabel(row.owner),
    up: row.up === true,
    windowed: row.windowed === true,
    busy: row.busy === true,
    closed: row.closed === true,
    // **The server's own mark for a row it could not fully read** (a corrupt
    // owner record): the row is drawn, and carries no verbs.
    unreadable: row.unreadable === true,
  };
}

/** The row a value that is not a profile takes: named by its place, and inert. */
function unreadable(at: number): ProfileRow {
  return {
    name: `(unreadable ${at + 1})`,
    owner: '',
    up: false,
    windowed: false,
    busy: false,
    closed: false,
    unreadable: true,
  };
}

/** A slot as the strip's own line reads it, or `''` when none came. */
function slotLabel(owner: unknown): string {
  if (typeof owner !== 'object' || owner === null) return '';
  const slot = owner as Record<string, unknown>;
  const parts = [slot.org, slot.project, slot.label];
  return parts.every((part) => typeof part === 'string') ? parts.join('/') : '';
}

/** Close a named profile: the strip's row, acting for the person. */
export async function closeProfile(name: string, seat: SessionSlot | null = null): Promise<void> {
  if (!canHost()) return;
  await invokeShell('client_browser_profile_close', { seat, name });
}

/** Delete a named profile: the one act that takes its logins. */
export async function deleteProfile(name: string, seat: SessionSlot | null = null): Promise<void> {
  if (!canHost()) return;
  await invokeShell('client_browser_profile_delete', { seat, name });
}

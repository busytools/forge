/**
 * The shell as the browser host: what answers a session's browser asks.
 *
 * The socket hands this connection the asks and the answer has to go back
 * under the ask's own id; the work in between is the shell's - the Rust side
 * owns the browser process, the profile and the driver - so this module is
 * the one place the two meet.
 *
 * **The capability is declared only where a host is really there.** A page
 * opened outside the shell (the dev server in a plain browser, where nothing
 * answers `invoke`) must not claim it: an ask routed to a client that cannot
 * serve it arrives as a session's tool call failing, which is a lie about the
 * session rather than about the page.
 */

import type { InvokeArgs } from '@tauri-apps/api/core';

import type { BrowserAnswer, BrowserAnswerPart, BrowserAsk } from '../protocol';
import type { Connection } from '../socket';

/** One part as the shell returns it: an image's bytes are base64 there. */
export type HostPart =
  { type: 'text'; text: string } | { type: 'image'; mime_type: string; data_base64: string };

/** What the shell's `browser_call` answers with. */
export interface HostReply {
  parts: HostPart[];
}

/** How a tool call reaches the shell, injectable so the mapping is testable. */
export type Invoke = (command: 'browser_call', request: InvokeArgs) => Promise<unknown>;

/** Whether this page runs inside the shell that owns a browser host. */
export function canHost(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
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

/**
 * One part as the socket wants it: an image's bytes in hand, not base64.
 *
 * Exported for the same reason the invoke is injectable: it is the mapping
 * the shell's contract rests on, so a test calls the real one rather than a
 * copy that can drift.
 */
export function answerPart(part: HostPart): BrowserAnswerPart {
  return part.type === 'image'
    ? { type: 'image', mime_type: part.mime_type, bytes: bytesOf(part.data_base64) }
    : part;
}

/**
 * Answer the asks this connection is sent, through the shell.
 *
 * A failed call is answered with its reason rather than thrown: the session's
 * tool call is waiting on this, and the reason is what it returns to the
 * model. The command's rejection carries the driver's own sentence, which is
 * the most useful thing anyone down that path can read.
 */
export function hostTheBrowser(connection: Connection, invoke: Invoke = defaultInvoke): () => void {
  return connection.onBrowserAsk(async (ask: BrowserAsk): Promise<BrowserAnswer> => {
    try {
      const reply = (await invoke('browser_call', {
        seat: ask.seat,
        tool: ask.tool,
        args: ask.args,
      })) as HostReply;
      return { parts: reply.parts.map(answerPart) };
    } catch (why) {
      return { error: whyText(why) };
    }
  });
}

/**
 * The reason a rejected command carried, as a sentence.
 *
 * Exported for the strip's own reads: a failed `listProfiles` or a refused
 * close has the shell's own words in its rejection, and a caller that drew
 * anything else would be inventing a reason.
 */
export function whyText(why: unknown): string {
  if (typeof why === 'string' && why !== '') return why;
  return `the client's browser host failed: ${String(why)}`;
}

/**
 * The real `invoke`, loaded lazily.
 *
 * Imported here rather than at the top so this module can be read, and its
 * mapping tested, without a Tauri process anywhere: the import is what fails
 * outside the shell, not the code that decides what to do with an answer.
 */
async function defaultInvoke(_command: 'browser_call', request: InvokeArgs): Promise<unknown> {
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke('browser_call', request);
}

/**
 * Bring the browser up visibly, which is what a hand-off's Open asks for:
 * the person's own browser window over the profile the agents drive
 * (headless until then).
 *
 * **Answers whether a browser was really raised.** `false` outside the shell
 * and on a failed raise - an unvendored build, a host that cannot start -
 * and the caller draws that truth rather than claiming a window is up. A
 * dock that said the browser is up over a raise that never happened would
 * have the person press Done and tell the session they acted.
 */
export async function showBrowser(): Promise<boolean> {
  if (!canHost()) return false;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('browser_show');
    return true;
  } catch {
    return false;
  }
}

/**
 * Take the hand-off's window back down: the browser closes, and the next
 * agent call relaunches it headless over the same profile. **The hand-off
 * itself is answered separately** - Done or Not now - and answering it
 * lowers the window, so the cycle opens and closes as one act.
 */
export async function hideBrowser(): Promise<void> {
  if (!canHost()) return;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('browser_hide');
  } catch {
    // A window that will not come down is the shell's to say; nothing here
    // can act on it.
  }
}

/**
 * Bring the in-app browser view up over the client's window: the takeover
 * the frames path draws (the surface kept for the platforms whose browsers
 * cannot have a window of their own).
 */
export async function openTakeover(): Promise<void> {
  if (!canHost()) throw new Error('this page is not the client');
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('browser_takeover_open');
}

/** One input event into the live view, as CDP wants it. */
export async function takeoverInput(method: string, params: unknown): Promise<void> {
  if (!canHost()) return;
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('browser_takeover_input', { method, params });
}

/** One frame of the live view, as the shell's `browser_frame` event carries it. */
export interface TakeoverFrame {
  data: string;
  width: number;
  height: number;
}

/**
 * The frame the shell holds, which the view reads on its own beat.
 *
 * **A pull, not a push.** The first frame of a static page is also its last,
 * and the event path across the process boundary is the piece observed to go
 * missing - so the screen asks for what is current rather than depending on
 * the next one arriving.
 */
export async function takeoverFrame(): Promise<TakeoverFrame | null> {
  if (!canHost()) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<TakeoverFrame | null>('browser_takeover_frame');
  } catch {
    return null;
  }
}

/** Take the view back down. Idempotent, and silent outside the shell. */
export async function closeTakeover(): Promise<void> {
  if (!canHost()) return;
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('browser_takeover_close');
}

/** What a reloaded window reads to re-draw the takeover it was on. */
export interface TakeoverState {
  /** Whether the takeover is up. */
  active: boolean;
  /**
   * Whether the picture is the browser's own view. Then the screen draws no
   * frames of its own - polling for them would be paying for nothing.
   */
  native: boolean;
}

export async function takeoverState(): Promise<TakeoverState> {
  if (!canHost()) return { active: false, native: false };
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<TakeoverState>('browser_takeover_state');
  } catch {
    return { active: false, native: false };
  }
}

/** Whether a session has driven this client's browser since it came up. */
export async function browserUsed(): Promise<boolean> {
  if (!canHost()) return false;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<boolean>('browser_used');
  } catch {
    return false;
  }
}

/** The page the browser is showing, for the takeover's bar. */
export async function takeoverUrl(): Promise<string | null> {
  if (!canHost()) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<string | null>('browser_takeover_url');
  } catch {
    return null;
  }
}

/**
 * Close a named profile from the client's own UI: the strip's row, acting
 * for the person rather than for a session - the door a profile whose
 * owning session is gone comes back through.
 */
export async function closeProfile(name: string): Promise<void> {
  if (!canHost()) return;
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('browser_profile_close', { name });
}

/** One named profile, as the client's own browser strip draws it. */
export interface ProfileRow {
  name: string;
  /** The slot of the session that opened it. */
  owner: string;
  /** Whether its driver is still there to answer. */
  running: boolean;
}

/**
 * The named profiles this client holds, for its own strip.
 *
 * The profiles are the client's own state - it owns the drivers - so this is
 * the client reading itself, and a page outside the shell holds none.
 */
export async function listProfiles(): Promise<ProfileRow[]> {
  if (!canHost()) return [];
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<ProfileRow[]>('browser_profiles');
}

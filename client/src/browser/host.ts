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
import { callDown, callUp } from './inflight.svelte';

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

/**
 * The phone's takeover bar speaks into this page: the Kotlin bar calls
 * `window.__forgeTakeover('done' | 'lowered')` on the client's own webview,
 * and the hook re-emits it as a window event - the dock answers `done` the
 * same way its own Done button does, and the strip re-reads on `lowered`.
 * The hook exists on every host; only the phone's bar ever calls it.
 */
export function installTakeoverHook(): void {
  if (typeof window === 'undefined') {
    return;
  }
  (window as unknown as { __forgeTakeover?: (what: string) => void }).__forgeTakeover = (
    what: string,
  ): void => {
    window.dispatchEvent(new CustomEvent('forge-takeover', { detail: what }));
  };
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
  installTakeoverHook();
  return connection.onBrowserAsk(async (ask: BrowserAsk): Promise<BrowserAnswer> => {
    // The strip's live mark: up while the call runs, so the row says "the
    // browser is working" from the same fact that makes it true.
    callUp();
    try {
      const reply = (await invoke('browser_call', {
        seat: ask.seat,
        tool: ask.tool,
        args: ask.args,
      })) as HostReply;
      return { parts: reply.parts.map(answerPart) };
    } catch (why) {
      return { error: whyText(why) };
    } finally {
      callDown();
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
 * (headless until then). **`profile` names which one**: `null` is the
 * shared profile, a name is that profile's own window over its own browser
 * - the CAPTCHA in the right session, not a lookalike in the wrong one.
 *
 * **Answers why it could not be raised, or `null` when it was.** The shell
 * carries the actionable sentence - a machine with no browser to drive is
 * told which one to install - and a caller that drew a reason of its own
 * would throw that away. Outside the shell the reason is the page's own.
 * A dock that claimed a window was up over a raise that never happened
 * would have the person press Done and tell the session they acted.
 */
export async function showBrowser(profile: string | null = null): Promise<string | null> {
  if (!canHost()) return 'this page is not the client';
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('browser_show', { profile });
    return null;
  } catch (why) {
    return whyText(why);
  }
}

/**
 * Take the hand-off's window back down: the browser closes, and the next
 * agent call relaunches it headless over the same profile. **The hand-off
 * itself is answered separately** - Done or Not now - and answering it
 * lowers the window, so the cycle opens and closes as one act.
 *
 * **Answers why it could not come down, or `null` when it did** - the same
 * shape a raise answers with, so a strip button that said hide and then
 * drew nothing on a refusal cannot happen.
 */
export async function hideBrowser(profile: string | null = null): Promise<string | null> {
  if (!canHost()) return 'this page is not the client';
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('browser_hide', { profile });
    return null;
  } catch (why) {
    return whyText(why);
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

/**
 * Whether a profile's browser is up as a window - the shared one for `null` -
 * which is what the strip's show/hide button toggles on.
 */
export async function profileWindowed(name: string | null = null): Promise<boolean> {
  if (!canHost()) return false;
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<boolean>('browser_windowed', { name });
}

/** One named profile, as the client's own browser strip draws it. */
export interface ProfileRow {
  name: string;
  /** The slot of the session that opened it. */
  owner: string;
  /** Whether its driver is still there to answer. */
  running: boolean;
  /** Whether its browser is up as a WINDOW right now: the row's button says
   *  hide where it is, and show where it is not. */
  windowed: boolean;
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

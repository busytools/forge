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
 * Exported for the strip's own reads: a failed `listContexts` or a refused
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
 * Bring the browser up visibly, which is what a hand-off's Open asks for.
 *
 * **Answers whether a browser was really raised.** `false` outside the shell
 * and on a failed raise - an unvendored build, a host that cannot start -
 * and the caller draws that truth rather than claiming a window is up. A
 * dock that said "The browser is up" over a raise that never happened would
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
 * Whether a browser window is already up.
 *
 * A dock asks before offering to open one: a button that says Open over a
 * window already open is a click that cannot do what it says - nothing can
 * raise another application's window - so the dock says the window is up
 * instead. `false` outside the shell, where nothing was raised.
 */
export async function browserWindowUp(): Promise<boolean> {
  if (!canHost()) return false;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    return await invoke<boolean>('browser_window');
  } catch {
    return false;
  }
}

/**
 * Close a named context from the client's own UI: the strip's row, acting
 * for the person rather than for a session - the door a context whose
 * owning session is gone comes back through.
 */
export async function closeContext(name: string): Promise<void> {
  if (!canHost()) return;
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('browser_context_close', { name });
}

/** One named context, as the client's own browser strip draws it. */
export interface ContextRow {
  name: string;
  /** The slot of the session that opened it. */
  owner: string;
  /** Whether its driver is still there to answer. */
  running: boolean;
}

/**
 * The named contexts this client holds, for its own strip.
 *
 * The contexts are the client's own state - it owns the drivers - so this is
 * the client reading itself, and a page outside the shell holds none.
 */
export async function listContexts(): Promise<ContextRow[]> {
  if (!canHost()) return [];
  const { invoke } = await import('@tauri-apps/api/core');
  return await invoke<ContextRow[]>('browser_contexts');
}

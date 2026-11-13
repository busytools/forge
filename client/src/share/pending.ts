/**
 * The share the app was opened with, waiting for the sheet to draw it.
 *
 * **Two doors, one payload.** A cold start's share is the activity's launch
 * intent, which the page asks for once on mount; a share into a running app
 * arrives as the shell's `client://share` event. Both carry the same files,
 * so the sheet has one reader.
 *
 * A share that carries no files is still an answer: the sheet draws the
 * reason rather than opening on nothing.
 */

import { get, writable } from 'svelte/store';

import { canHost } from '../browser/host';
import { currentRoute, goTo } from '../routes';
import type { Shared } from './upload';

/** The files a share handed this app, or `null` when none is waiting. */
export const pendingShare = writable<Shared | null>(null);

/** A payload as a share, or `null` when it names no share at all. */
export function shareOf(value: unknown): Shared | null {
  if (typeof value !== 'object' || value === null) return null;
  const held = value as Record<string, unknown>;
  const files = Array.isArray(held['files']) ? held['files'] : null;
  if (files === null) return null;
  const rows = files.flatMap((row) => {
    if (typeof row !== 'object' || row === null) return [];
    const file = row as Record<string, unknown>;
    if (typeof file['path'] !== 'string' || typeof file['name'] !== 'string') return [];
    return [
      {
        path: file['path'],
        name: file['name'],
        size: typeof file['size'] === 'number' ? file['size'] : 0,
        mime: typeof file['mime'] === 'string' ? file['mime'] : null,
      },
    ];
  });
  const error = typeof held['error'] === 'string' ? held['error'] : null;
  const empty = held['empty'] === true;
  // **A payload whose rows all failed the narrowing is a refusal too**: the
  // sheet says so rather than opening on a share with nothing in it.
  if (rows.length === 0 && files.length > 0 && error === null) {
    return { files: [], error: 'this share named files this client cannot read', empty: false };
  }
  return { files: rows, error, empty };
}

/**
 * Whether a payload is a share worth opening the sheet for.
 *
 * **A payload with neither files nor a reason is not one**: the Kotlin half
 * answers an empty payload for every launch that shared nothing, and opening
 * the sheet on that would put a dead page in front of every cold start.
 */
export function worthOpening(shared: Shared): boolean {
  // **A share of nothing is still a share**: the sheet opens to say it
  // carried no file, rather than the launch passing for one that shared
  // nothing at all.
  return shared.files.length > 0 || shared.error !== null || shared.empty;
}

/**
 * The share this launch came from, asked for before the page's first draw.
 *
 * `true` means a share is waiting and the caller moves the reader to the
 * sheet; `false` is every launch that shared nothing, the desktop included.
 */
export async function launchShare(): Promise<boolean> {
  if (!canHost()) return false;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const shared = shareOf(await invoke<unknown>('client_share_take'));
    if (shared === null || !worthOpening(shared)) return false;
    pendingShare.set(shared);
    return true;
  } catch (why) {
    // A launch whose share cannot be read is still a launch: the page opens
    // where it always did rather than holding on a command.
    console.warn('the share this launch came from could not be read', why);
    return false;
  }
}

/**
 * A share into an app that is already running.
 *
 * The shell pushes it; the sheet opens over whatever the reader was on, and
 * Back closes it (the route is a history entry like every other page).
 */
export async function watchShares(): Promise<() => void> {
  if (!canHost()) return () => {};
  const { listen } = await import('@tauri-apps/api/event');
  return listen<unknown>('client://share', (event) => {
    const shared = shareOf(event.payload);
    if (shared === null || !worthOpening(shared)) return;
    pendingShare.set(shared);
    // **A share that arrives while the sheet is already open replaces it in
    // place**: the keyed sheet remounts on the store, and a second `/share`
    // entry would make the first Back stop on the page it already left.
    if (get(currentRoute).name === 'share') return;
    // **A share outranks a page's hold**: the sheet is the system handing
    // forge a file, and the draft the hold protects stays in its box. A
    // held move would swallow the share into a store no page draws.
    goTo({ name: 'share' }, { force: true });
  });
}

/**
 * Tell the shell's share intake this page's listener is registered.
 *
 * **The shell's channel is not this listener**: it is up at plugin setup and
 * a push into a webview with no `listen` is dropped silently, so the Kotlin
 * half buffers a warm share until this arrives. Best-effort: a host without
 * the Android plugin answers nothing, and a share already buffered is
 * answered by the launch ask either way.
 */
export async function readyForShares(): Promise<void> {
  if (!canHost()) return;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('client_share_ready');
  } catch (why) {
    console.warn('the share intake was not told the page is listening', why);
  }
}

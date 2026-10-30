/**
 * The settings frame every connection is handed after its greeting.
 *
 * The server pushes it on attach and again on every applied reload, so the
 * page and the client's own axes read one held document rather than a read
 * verb. `waiting` is "no frame yet" - the page's loading state - and
 * `failed` is a frame this client could not read, drawn as itself rather
 * than as an empty config.
 */

import { writable } from 'svelte/store';

import type { ClientSettings } from '../wire/types';
import { settingsDocumentFrom, type SettingsDocument } from '../wire/settings';

export type SettingsFrame =
  | { status: 'waiting' }
  | { status: 'ready'; document: SettingsDocument }
  | { status: 'failed'; detail: string };

export const settingsFrame = writable<SettingsFrame>({ status: 'waiting' });

/**
 * The latest frame each connection was handed, and which one the shell has
 * taken.
 *
 * **A connection's frame belongs to it.** A submit can leave a launch's
 * socket delivering its own greeting and document after the person's
 * connection took the page; without this, that stale pair would re-theme the
 * page the submit is drawing.
 */
const heldBy = new WeakMap<object, SettingsFrame>();
let activeOwner: object | null = null;

/** Take one pushed frame, for the connection that received it. */
export function takeSettings(value: unknown, owner: object): void {
  const document = settingsDocumentFrom(value);
  const frame: SettingsFrame =
    document === null
      ? { status: 'failed', detail: 'This forge sent a settings frame this client could not read.' }
      : { status: 'ready', document };
  heldBy.set(owner, frame);
  if (owner === activeOwner) settingsFrame.set(frame);
}

/**
 * The shell has taken this connection: draw its document, or wait for the
 * one that follows its greeting. A frame that landed before the take is
 * kept, so the page never waits for a push that already arrived.
 */
export function ownSettingsFrame(owner: object): void {
  activeOwner = owner;
  settingsFrame.set(heldBy.get(owner) ?? { status: 'waiting' });
}

/**
 * The client's own axes, as the settings frame carries them. `null` until a
 * document has landed: a client that never received one applies nothing and
 * keeps the built-in sets.
 */
export function clientAxes(
  frame: SettingsFrame,
): Pick<ClientSettings, 'mark' | 'theme' | 'font'> | null {
  if (frame.status !== 'ready') return null;
  const values = new Map<string, string>();
  for (const area of frame.document.areas) {
    if (area.id !== 'client') continue;
    for (const key of area.keys) {
      if (key.value.kind === 'string') values.set(key.path, key.value.text);
    }
  }
  return {
    mark: values.get('client.mark') ?? null,
    theme: values.get('client.theme') ?? null,
    font: values.get('client.font') ?? null,
  };
}

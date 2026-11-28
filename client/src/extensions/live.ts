/**
 * The Extensions page over a live connection: the record its subscribe answer
 * carries, and every patch the socket applies into it after that.
 *
 * **The page draws the store and nothing else.** The `Plugins` family used to
 * be answered with a whole re-read because a run's frame carries the run and
 * no inventory, an action's outcome carries both in its own shape, and the
 * PATH checks are a server scan no frame carries. The server now patches its
 * own record when those move, so the read path and the live path are one
 * shape by construction (rule 25) and the page never asks for a scan.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { extensionsFrom, type ExtensionsRead } from '../wire/extensions';
import type { Subject } from '../protocol';

/** The Extensions page's subject, watched by this page and by the home's
 * extensions card. */
const EXTENSIONS: Subject = 'extensions';

/** What the page has to draw from, and why it has nothing when it does not. */
export interface ExtensionsPage {
  read: ExtensionsRead | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
  /** Patches the socket could not place, held for the page to say so. */
  heldPatches?: unknown[];
  /** The stream skipped a version: the record stays drawn while one read lands. */
  resyncing?: boolean;
}

const NOTHING: ExtensionsPage = { read: null, refused: null };

/** What watching the extensions needs of a socket. */
type ExtensionsConnection = Pick<Connection, 'subscribe' | 'unsubscribe' | 'onStatus'>;

/**
 * Watch the Extensions page.
 *
 * The subscription and the status listener are let go with the last
 * subscriber, so a page the reader has left is not left subscribed.
 */
export function watchExtensions(connection: ExtensionsConnection): Readable<ExtensionsPage> {
  let held: Store | null = null;
  let stopStore: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;
  /** What the last draw was made from; the store notifies for frames too. */
  let seenSnapshot: unknown = null;
  let seenVersion: number | null = null;
  let seenState: string | null = null;
  let seenHeld = 0;

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      view.set({
        read: null,
        refused: state.why,
        heldPatches: held.unexpected(),
        resyncing: false,
      });
      return;
    }
    const data = held.snapshot();
    view.set({
      read: data === null ? null : extensionsFrom(data),
      refused: null,
      heldPatches: held.unexpected(),
      resyncing: state.kind === 'resyncing',
    });
  }

  function watch(): void {
    held = connection.subscribe(EXTENSIONS);
    stopStore = held.value.subscribe(() => {
      if (held === null) return;
      const snapshot = held.snapshot();
      const version = held.version();
      const state = held.state().kind;
      const heldPatches = held.unexpected().length;
      if (
        snapshot === seenSnapshot &&
        version === seenVersion &&
        state === seenState &&
        heldPatches === seenHeld
      ) {
        return;
      }
      seenSnapshot = snapshot;
      seenVersion = version;
      seenState = state;
      seenHeld = heldPatches;
      read();
    });
    // A drop leaves the record in place; the reconnect's answer re-seeds the
    // store and the subscription above redraws it.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') seenSnapshot = null;
    });
  }

  function unwatch(): void {
    stopStore?.();
    stopStatus?.();
    stopStore = null;
    stopStatus = null;
    connection.unsubscribe(EXTENSIONS);
    held = null;
  }

  const view: Writable<ExtensionsPage> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

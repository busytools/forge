/**
 * The models page over a live connection: the record its subscribe answer
 * carries, and every patch the socket applies into it after that.
 *
 * **One path, because the server sends one.** A catalogue check landed and a
 * model finished loading both move the server's own record, and the patch
 * that answers either carries what moved - so the page draws the store and
 * never asks for a catalogue encode. A check's frame already carried the
 * whole read; a load's signal carried nothing, which is why it used to cost a
 * read. Neither does now.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { modelsFrom, type DictateModelsWire } from '../wire/models';
import type { Subject } from '../protocol';

/** The models page's subject. This page watches it for its whole life; the
 * settings page watches it too, only while its voice area is open. */
const MODELS: Subject = 'dictate_models';

/** What the models page has to draw from, and why it has nothing when it does not. */
export interface ModelsRead {
  wire: DictateModelsWire | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
  /** Patches the socket could not place, held for the page to say so. */
  heldPatches?: unknown[];
  /** The stream skipped a version: the record stays drawn while one read lands. */
  resyncing?: boolean;
}

const NOTHING: ModelsRead = { wire: null, refused: null };

/**
 * What watching the models needs of a socket: the subject, the unsubscribe
 * that goes with it, and the status it draws a drop from. The composer's
 * panel watches from its own seat, which is why the contract is stated here
 * rather than taking the whole connection.
 */
type ModelsConnection = Pick<Connection, 'subscribe' | 'unsubscribe' | 'onStatus'>;

/**
 * Watch the models page.
 *
 * The subscription and the status listener are let go with the last
 * subscriber, so a page the reader has left is not left subscribed.
 */
export function watchModels(connection: ModelsConnection): Readable<ModelsRead> {
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
        wire: null,
        refused: state.why,
        heldPatches: held.unexpected(),
        resyncing: false,
      });
      return;
    }
    const data = held.snapshot();
    // The snapshot is JSON the server wrote from its own type, and
    // `modelsFrom` narrows every union member in it straight afterwards.
    view.set({
      wire: data === null ? null : modelsFrom(data as unknown as DictateModelsWire),
      refused: null,
      heldPatches: held.unexpected(),
      resyncing: state.kind === 'resyncing',
    });
  }

  function watch(): void {
    held = connection.subscribe(MODELS);
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
    connection.unsubscribe(MODELS);
    held = null;
  }

  const view: Writable<ModelsRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

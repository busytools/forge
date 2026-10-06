/**
 * The models page over a live connection: the snapshot the subject is
 * answered with, the check that lands as an update, and the load's signal.
 *
 * **Two live paths, because the server sends two different things.** A check
 * landing arrives as `DictateModelsChanged` carrying the whole read, so the
 * page takes it as it stands and asks for nothing; a model finishing its load
 * arrives as `DictateAvailability`, which is a bare signal with no payload, so
 * that one is answered with a read. A page that answered both with a read
 * would ask the server to rebuild what one of them already holds; one that
 * answered both from the update would keep drawing `pending` chips after the
 * models loaded.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { report, type Connection, type ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { modelsFrom, type DictateModelsWire } from '../wire/models';
import type { Subject } from '../protocol';

/** The models page's subject, which only this page watches. */
const MODELS: Subject = 'dictate_models';

/** What the models page has to draw from, and why it has nothing when it does not. */
export interface ModelsRead {
  wire: DictateModelsWire | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: ModelsRead = { wire: null, refused: null };

/**
 * Watch the models page.
 *
 * The subscription, the message listener and the status listener are all let
 * go with the last subscriber, so a page the reader has left stops asking a
 * forge to encode a catalogue nobody draws.
 */
export function watchModels(connection: Connection): Readable<ModelsRead> {
  let held: Store | null = null;
  // A read is a full encode on the server, so one already in flight is the
  // fresher answer and a second is not queued behind it.
  let reading = false;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      view.set({ wire: null, refused: state.why });
      return;
    }
    const data = held.snapshot();
    // The snapshot is JSON the server wrote from its own type, and
    // `modelsFrom` narrows every union member in it straight afterwards.
    view.set({
      wire: data === null ? null : modelsFrom(data as unknown as DictateModelsWire),
      refused: null,
    });
  }

  function watch(): void {
    held = connection.subscribe(MODELS);
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        if (message.kind === 'error' || message.subject === MODELS) {
          reading = false;
          read();
        }
        return;
      }
      if (message.kind !== 'update') return;
      // The connection carries every subject this client watches - the shell
      // holds the home on it too - so the two variants this page acts on are
      // the filter, not the stream.
      const variant =
        typeof message.update === 'string' ? message.update : Object.keys(message.update)[0];
      if (variant === 'dictate_models_changed' && typeof message.update !== 'string') {
        // The landing carries the whole read: taking it is the page's copy of
        // the check, with no second encode asked for.
        const payload = message.update[variant] as { models?: DictateModelsWire } | undefined;
        if (payload?.models === undefined) {
          // A landing with nothing in it is a page that stops following the
          // catalogue, which is worth a line rather than a silence.
          report('a catalogue check landed carrying no models', message.update);
          return;
        }
        view.set({ wire: modelsFrom(payload.models), refused: null });
        return;
      }
      if (variant !== 'dictate_availability') return;
      // A signal with no payload: the read is the only way to see what moved.
      if (reading) return;
      reading = true;
      connection.refresh(MODELS);
    });
    // A drop takes any read in flight with it, and the reconnect answers with
    // a snapshot of its own - so the pacing must not stay stuck waiting for an
    // answer that died.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') reading = false;
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
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

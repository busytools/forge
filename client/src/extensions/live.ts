/**
 * The Extensions page over a live connection: the read the subject is
 * answered with, and the frames the `Plugins` family pushes.
 *
 * **One drawable shape, two paths to it.** The live frames do not all
 * carry the same half of the page - a run's progress carries the run and
 * no inventory, an action's outcome carries both in its own shape - so the
 * page takes what a frame HAS (a run's rows move the rows in place) and
 * re-reads for the rest. A refresh answers both with the one shape the
 * page draws, so a run's row and a read's row can never disagree (rule 25:
 * the read path and the live path are one obligation).
 *
 * The PATH checks are the reason the run's own snapshot is not lifted off
 * the frame: the scan cannot carry them, and a page that took the snapshot
 * would keep drawing a check for a command that is no longer there. The
 * read recomputes them.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import { report, type Connection, type ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { extensionsFrom, updateRunFrom, type ExtensionsRead } from '../wire/extensions';
import type { Subject } from '../protocol';

/** The Extensions page's subject, watched by this page and by the home's
 * extensions card. */
const EXTENSIONS: Subject = 'extensions';

/** What the page has to draw from, and why it has nothing when it does not. */
export interface ExtensionsPage {
  read: ExtensionsRead | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: ExtensionsPage = { read: null, refused: null };

/** What watching the extensions needs of a socket. */
export type ExtensionsConnection = Pick<
  Connection,
  'subscribe' | 'unsubscribe' | 'onMessage' | 'onStatus' | 'refresh'
>;

/**
 * Watch the Extensions page.
 *
 * The subscription, the message listener and the status listener are all
 * let go with the last subscriber, so a page the reader has left stops
 * asking a forge to scan a plugin tree nobody draws.
 */
export function watchExtensions(connection: ExtensionsConnection): Readable<ExtensionsPage> {
  let held: Store | null = null;
  // A read is a whole disk scan on the server, so one already in flight is
  // the fresher answer and a second is not queued behind it.
  let reading = false;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;
  let last: ExtensionsRead | null = null;

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      last = null;
      view.set({ read: null, refused: state.why });
      return;
    }
    const data = held.snapshot();
    last = data === null ? null : extensionsFrom(data);
    view.set({ read: last, refused: null });
  }

  function refresh(): void {
    if (reading) return;
    if (held === null) return;
    reading = true;
    connection.refresh(EXTENSIONS);
  }

  function watch(): void {
    held = connection.subscribe(EXTENSIONS);
    reading = false;
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        if (message.kind === 'error' || message.subject === EXTENSIONS) {
          reading = false;
          read();
        }
        return;
      }
      if (message.kind !== 'update') return;
      // The connection carries every subject this client watches, so the
      // variants this page acts on are the filter, not the stream.
      const variant =
        typeof message.update === 'string' ? message.update : Object.keys(message.update)[0];
      if (variant === 'plugins_update_run_progress' && typeof message.update !== 'string') {
        // The frame carries the run and nothing else, so its rows move the
        // page's copy in place: no disk scan for a row ticking over.
        const payload = message.update[variant] as { run?: unknown } | undefined;
        const run = updateRunFrom(payload?.run ?? null);
        if (run === null) {
          report('a plugin run ticked with no run in it', message.update);
          return;
        }
        const base: ExtensionsRead = last ?? { snapshot: null, update_run: null, lsp: [] };
        last = { ...base, update_run: run };
        view.set({ read: last, refused: null });
        return;
      }
      if (
        variant === 'plugins_update_run_finished' ||
        variant === 'plugins_inventory_updated' ||
        variant === 'plugins_inventory_refresh_failed' ||
        variant === 'plugins_cli_action_succeeded' ||
        variant === 'plugins_cli_action_failed' ||
        variant === 'plugins_rollback_succeeded' ||
        variant === 'plugins_rollback_failed'
      ) {
        reading = false;
        refresh();
        return;
      }
    });
    // A drop takes any read in flight with it, and the reconnect answers
    // with a snapshot of its own: the pacing must not stay stuck waiting
    // for an answer that died.
    stopStatus = connection.onStatus((next: ConnectionStatus) => {
      if (next !== 'open') reading = false;
    });
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
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

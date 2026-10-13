/**
 * The usage page over a live connection: the snapshot the subject is
 * answered with, and the refusal when it is turned down.
 *
 * **One answer and no updates.** The subject scans the pool on the ask and
 * hears nothing after it - the pool changes as transcripts are written, and
 * nothing in the stream says a transcript's tokens moved - so a page that
 * waited for an update would wait for ever. Fresh figures are asked for
 * again: the page's refresh control calls `connection.refresh('usage')`, and
 * its answer arrives here as another snapshot on the same subject.
 */

import { writable, type Readable, type Writable } from 'svelte/store';

import type { Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store } from '../stores';
import { usageFrom, type UsageReport } from '../wire/usage';

/** The usage page's subject, which only this page watches. */
const USAGE: Subject = 'usage';

/** What the usage page has to draw from, and why it has nothing when it does not. */
export interface UsageRead {
  wire: UsageReport | null;
  /** The server's own words for turning the subscription down, or `null`. */
  refused: string | null;
}

const NOTHING: UsageRead = { wire: null, refused: null };

/**
 * How long after an unpriced report the page asks once more.
 *
 * **One re-ask, because the server's answer to an unpriced scan is a
 * background fetch.** The ask that scanned also kicked the pricing refill,
 * which re-scans with the fetched table and leaves the priced report in the
 * cache - but nothing pushes, so the fresh figures are only ever seen by
 * asking again. This is the terminal's own re-price (it draws a fresh scan
 * when one arrives) translated into the one move this subject offers. A
 * delay past the usual fetch and re-scan; `pricing pending or failed` is a
 * terminal caption, so a failing fetch asks exactly once and stops rather
 * than polling.
 */
export const PRICING_RETRY_MS = 6_000;

/**
 * What watching the usage needs of a socket: the subject, its pushes, the
 * re-read the unpriced re-ask sends, and the unsubscribe that goes with the
 * subscribe. Stated here rather than taking the whole connection, the way
 * every page's watch does.
 */
export type UsageConnection = Pick<
  Connection,
  'subscribe' | 'unsubscribe' | 'onMessage' | 'onStatus' | 'refresh'
>;

/**
 * Watch the usage subject.
 *
 * The subscription, the message listener and the status listener are all let
 * go with the last subscriber, so a page the reader has left stops asking a
 * forge to walk the pool nobody draws.
 */
export function watchUsage(connection: UsageConnection): Readable<UsageRead> {
  let held: Store | null = null;
  let stopMessages: (() => void) | null = null;
  let stopStatus: (() => void) | null = null;
  // Whether the unpriced re-ask has been spent. Latched when the timer is
  // SET, so a second unpriced answer arriving before it fires cannot
  // schedule a second one.
  let reasked = false;
  let retry: ReturnType<typeof setTimeout> | null = null;

  function read(): void {
    if (held === null) return;
    const state = held.state();
    if (state.kind === 'refused') {
      view.set({ wire: null, refused: state.why });
      return;
    }
    const data = held.snapshot();
    // The snapshot is JSON the server wrote from its own type; `usageFrom`
    // narrows every field straight afterwards.
    const wire = data === null ? null : usageFrom(data);
    view.set({ wire, refused: null });
    if (wire !== null && !wire.pricingAvailable && !reasked) {
      reasked = true;
      retry = setTimeout(() => {
        retry = null;
        connection.refresh(USAGE);
      }, PRICING_RETRY_MS);
    }
  }

  function watch(): void {
    held = connection.subscribe(USAGE);
    stopMessages = connection.onMessage((message) => {
      if (message.kind === 'snapshot' || message.kind === 'error') {
        // Every subject this client watches rides the same connection, so
        // the filter is the subject - home's frames are the shell's.
        if (message.kind === 'error' || message.subject === USAGE) read();
      }
    });
    // A drop loses the answer in flight with it; the next read is the page's
    // ask, and nothing here paces one, so the status listener only exists to
    // let a later snapshot land - kept for symmetry with the other watches.
    stopStatus = connection.onStatus((_next: ConnectionStatus) => {});
  }

  function unwatch(): void {
    stopMessages?.();
    stopStatus?.();
    stopMessages = null;
    stopStatus = null;
    // A page the reader has left must not ask for a scan nobody draws.
    if (retry !== null) clearTimeout(retry);
    retry = null;
    connection.unsubscribe(USAGE);
    held = null;
  }

  const view: Writable<UsageRead> = writable(NOTHING, () => {
    watch();
    read();
    return unwatch;
  });

  return { subscribe: view.subscribe };
}

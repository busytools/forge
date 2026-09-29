/**
 * The home over a live connection: the snapshot it is answered with, and the
 * read that follows every fleet-news update.
 *
 * **Nothing is folded here, and that is the point.** A row's state is the
 * server's, folded from the core's own `Live` alongside the roster, so it
 * arrives in the row already decided - including the unseen marks. A client
 * that recomputed any of it from what it happens to hold would disagree with
 * the terminal the first time a turn settled while nobody was watching, and
 * it would disagree silently. So an update the fleet cares about is answered
 * with a fresh read rather than with arithmetic, and an update it does not
 * care about is dropped.
 */

import { writable, type Readable } from 'svelte/store';

import type { Connection } from '../socket';
import { fleetNews } from '../wire/fleet';
import { homeFrom, type HomeWire } from '../wire/home';

/**
 * How long a fleet-news update waits before it is answered with a read.
 *
 * A burst becomes one read rather than one each: a turn settling emits
 * several at once, and a read answers with the fleet as it stands rather
 * than as it was at the first of them.
 */
const COALESCE_MS = 50;

/**
 * Watch the home. The connection is subscribed for the life of the store,
 * which is the life of the app: the home is the page behind every other.
 */
export function watchHome(connection: Connection): Readable<HomeWire | null> {
  const held = connection.subscribe('home');
  const view = writable<HomeWire | null>(null);
  let pending: ReturnType<typeof setTimeout> | null = null;

  function read(): void {
    const data = held.snapshot();
    // The snapshot is JSON the server wrote from its own type, and `homeFrom`
    // narrows every union member in it straight afterwards.
    view.set(data === null ? null : homeFrom(data as unknown as HomeWire));
  }

  connection.onMessage((message) => {
    if (message.kind === 'snapshot') {
      if (message.subject === 'home') read();
      return;
    }
    if (message.kind !== 'update' || fleetNews(message.update).kind === 'nothing') return;
    if (pending !== null) return;
    pending = setTimeout(() => {
      pending = null;
      connection.refresh('home');
    }, COALESCE_MS);
  });

  read();
  return { subscribe: view.subscribe };
}

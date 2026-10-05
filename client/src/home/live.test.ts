import { describe, expect, it } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { SessionSlot } from '../wire/types';
import { watchHome } from './live';

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** An update a home subscriber is sent, which is what it answers with a read. */
const UPDATE: ServerMessage = { kind: 'update', update: { turn_cancelled: { key: LEAD } } };

/**
 * A connection a test drives by hand: what the home asked it to do, and a way
 * to hand it a frame.
 *
 * The stores are the real ones, because a subscription's lifecycle is what
 * this fakes around rather than anything about a store.
 */
function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection: Connection = {
    subscribe(what) {
      subscribed.push(what);
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    refresh(what) {
      refreshed.push(what);
    },
    onMessage(fn) {
      messages.add(fn);
      return () => {
        messages.delete(fn);
      };
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    dispatch: () => null,
    more: () => false,
    devices: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    status: () => 'open',
    close: () => {},
  };

  return {
    connection,
    subscribed,
    unsubscribed,
    refreshed,
    /** Everything still attached to the connection. */
    listening: () => messages.size + statuses.size,
    /** One frame as the server sent it, into whatever is still listening. */
    arrive(message: ServerMessage) {
      for (const fn of [...messages]) fn(message);
    },
  };
}

/** Give a timer a chance to fire, for everything it would have set off to show. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 100));
}

describe('the home over a connection', () => {
  /**
   * A read is a full encode on the server, so a subscription nobody draws from
   * must not be left asking for one. The last subscriber leaving is what
   * releases the subscription, the listeners and any read already queued: a
   * replaced connection otherwise stays counted against a seat it is not
   * showing, re-reading the fleet for a page that is gone.
   */
  it('stops asking for the home once the last subscriber goes', async () => {
    const forge = fakeConnection();
    const stop = watchHome(forge.connection).subscribe(() => {});

    // While it is watched: an update is answered with one read, and the answer
    // to that read is what lets the next one be asked for.
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a watched home did not answer an update with a read').toEqual([
      'home',
    ]);
    forge.arrive({ kind: 'snapshot', subject: HOME, data: null });

    // The release, with a read already queued behind the last subscriber.
    forge.arrive(UPDATE);
    stop();
    expect(forge.unsubscribed, 'the last subscriber left the home subscribed').toEqual(['home']);

    // The queued read does not go, and no frame after it can ask for one.
    await settle();
    expect(forge.refreshed, 'a read queued behind the last subscriber still went').toHaveLength(1);
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a frame still reached a released subscription').toHaveLength(1);

    // And nothing is left attached to the connection, the status listener
    // included.
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});

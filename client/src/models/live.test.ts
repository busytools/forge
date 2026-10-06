import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { DictateModelsWire } from '../wire/models';
import { watchModels } from './live';

const MODELS: Subject = 'dictate_models';

/** A payload as `Subject::DictateModels` is answered with. */
const SNAPSHOT = {
  enabled: true,
  models_dir: '/tmp/models',
  in_use: [],
  check: { state: 'never' },
  updates: [],
  rows: [],
} as unknown as DictateModelsWire;

/**
 * A connection a test drives by hand, the same shape the home's own live test
 * fakes: the real stores, and a way to hand it a frame.
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
    skew: () => null,
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
    /**
     * One frame as the server sent it: the store is written the way the
     * socket writes it first, so a read of it is the answer the server gave,
     * and then every listener hears it.
     */
    arrive(message: ServerMessage) {
      if (message.kind === 'snapshot') {
        stores.get(message.subject)?.set(message.data);
      }
      if (message.kind === 'error' && message.what === 'subscribe') {
        stores.get(MODELS)?.refuse(message.why);
      }
      for (const fn of [...messages]) fn(message);
    },
  };
}

/** Give a timer a chance to fire, for everything it would have set off to show. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 100));
}

describe('the models page over a connection', () => {
  it('subscribes the models subject for as long as it is watched', () => {
    const forge = fakeConnection();
    watchModels(forge.connection).subscribe(() => {});

    expect(forge.subscribed).toEqual([MODELS]);
  });

  /**
   * The subscription's own answer is the first read, and it is narrowed at the
   * boundary before any page sees it.
   */
  it('takes the snapshot the subscription is answered with', () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    const stop = view.subscribe(() => {});

    forge.arrive({ kind: 'snapshot', subject: MODELS, data: SNAPSHOT });

    const held = get(view);
    expect(held.wire?.enabled).toBe(true);
    expect(held.wire?.in_use).toEqual([]);
    expect(held.refused).toBeNull();
    stop();
  });

  /**
   * **A landed check carries the whole read, so no second encode is asked
   * for.** The update is the page's one live path to a fresh catalogue, and a
   * client that answered it with a refresh would ask the server to rebuild
   * what the update already holds.
   */
  it('takes the snapshot a landed check carries, without asking for a read', async () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});

    const landed = {
      ...SNAPSHOT,
      check: { state: 'fresh', at: '2026-10-06T06:12:00Z', release: 'v0.3.1', skipped: 0 },
    };
    forge.arrive({ kind: 'update', update: { dictate_models_changed: { models: landed } } });
    await settle();

    expect(get(view).wire?.check).toEqual({
      state: 'fresh',
      at: '2026-10-06T06:12:00Z',
      release: 'v0.3.1',
      skipped: 0,
    });
    expect(forge.refreshed, 'a check landed with the read in hand, so nothing was re-read').toEqual(
      [],
    );
  });

  /**
   * **The load's own signal has no payload, so it is answered with a read.**
   * A page open through a first download would otherwise keep drawing
   * `pending` chips after the models finished loading.
   */
  it('re-reads when the models finish loading', async () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});

    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();

    expect(forge.refreshed).toEqual([MODELS]);
  });

  /**
   * A read is the whole subject, so one already in flight is not queued
   * behind: the answer it is waiting for is the fresher one.
   */
  it('asks for one read at a time, however many availability flips land', async () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});

    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();
    expect(forge.refreshed).toEqual([MODELS]);

    // The answer frees the pacing, so the next flip is asked for.
    forge.arrive({ kind: 'snapshot', subject: MODELS, data: SNAPSHOT });
    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();
    expect(forge.refreshed).toEqual([MODELS, MODELS]);
  });

  /** A refusal is the server's own words, and the page draws them. */
  it("keeps the server's reason for turning the subscription down", () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});

    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no models on this forge' });

    expect(get(view).refused).toBe('no models on this forge');
  });

  /**
   * The last subscriber leaving releases the subscription and the listeners:
   * a page gone must not leave a forge holding a subscription nothing draws,
   * nor a listener that answers the next flip with a read.
   */
  it('stops asking once the last subscriber goes', async () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    const stop = view.subscribe(() => {});

    stop();
    expect(forge.unsubscribed).toEqual([MODELS]);

    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();
    expect(forge.refreshed, 'a frame still reached a released subscription').toEqual([]);
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});

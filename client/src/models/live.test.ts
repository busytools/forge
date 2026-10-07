import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import type { DictateModelsWire } from '../wire/models';
import { watchModels } from './live';
import { fakeConnection, MODELS } from './testing';

/** A payload as `Subject::DictateModels` is answered with. */
const SNAPSHOT = {
  enabled: true,
  models_dir: '/tmp/models',
  in_use: [],
  check: { state: 'never' },
  updates: [],
  rows: [],
  install: { state: 'idle' },
  activate: { state: 'idle' },
  installed: [],
  bench: { state: 'idle' },
  results: [],
  read_aloud: { recorded: false, armed: false, passage: '' },
} as unknown as DictateModelsWire;

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

  /**
   * **A landing IS a read, so it frees the pacing.** A check that lands while
   * an availability read is out carries the whole snapshot that read was
   * asked for; if the landing left `reading` set, the pacing would latch -
   * that read's answer aside, no later flip could ever ask again, and the
   * page would stop following loads silently.
   */
  it('frees the read pacing when a check lands', async () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});

    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();
    expect(forge.refreshed).toEqual([MODELS]);

    forge.arrive({ kind: 'update', update: { dictate_models_changed: { models: SNAPSHOT } } });
    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    await settle();

    expect(forge.refreshed, 'a landing left the pacing latched').toEqual([MODELS, MODELS]);
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

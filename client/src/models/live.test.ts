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
  /**
   * **A landed check moves the page through its patch**, with no second
   * encode asked for: the server re-read its own record and sent what moved.
   */
  it('draws the check a landed patch carries, without asking for a read', () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});
    forge.arrive({ kind: 'snapshot', subject: MODELS, version: 1, data: SNAPSHOT });

    forge.arrive({
      kind: 'patch',
      subject: MODELS,
      version: 2,
      data: {
        check: { state: 'fresh', at: '2026-10-06T06:12:00Z', release: 'v0.3.1', skipped: 0 },
      },
    });

    expect(get(view).wire?.check).toEqual({
      state: 'fresh',
      at: '2026-10-06T06:12:00Z',
      release: 'v0.3.1',
      skipped: 0,
    });
    expect(forge.refreshed, 'a landed check asked for a whole read').toEqual([]);
  });

  /**
   * **The load's own signal carries no payload**, and the record moved when
   * the models loaded - so the page draws that move from the patch and asks
   * nothing.
   */
  it('draws a load from the patch, asking nothing', () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    view.subscribe(() => {});
    forge.arrive({
      kind: 'snapshot',
      subject: MODELS,
      version: 1,
      data: { ...SNAPSHOT, in_use: [{ role: 'transcribing', state: 'pending' }] },
    });
    forge.arrive({
      kind: 'patch',
      subject: MODELS,
      version: 2,
      data: { in_use: [{ role: 'transcribing', state: 'ready' }] },
    });
    expect(get(view).wire?.in_use?.[0]?.state).toBe('ready');
    expect(forge.refreshed).toEqual([]);
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
  it('stops asking once the last subscriber goes', () => {
    const forge = fakeConnection();
    const view = watchModels(forge.connection);
    const stop = view.subscribe(() => {});

    stop();
    expect(forge.unsubscribed).toEqual([MODELS]);

    forge.arrive({ kind: 'update', update: 'dictate_availability' });
    expect(forge.refreshed, 'a frame still reached a released subscription').toEqual([]);
    expect(forge.listening(), 'the release left a listener on the connection').toBe(0);
  });
});

// @vitest-environment jsdom
/**
 * The microphone hold at the router: which page opens it, and for which seat.
 *
 * **The decision lives in an effect, and a server render runs none**, so the
 * other Router suite cannot see it. What is pinned here is the wiring only -
 * that the page taking typing hands its seat's pick to the shell - and every
 * part of the page that is not the hold is either refused or left empty.
 */

import { mount, flushSync, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/** The shell, as the page reaches it: the holds it was asked for, and no more. */
const shell = vi.hoisted(() => {
  const holds: (string | null)[] = [];
  return {
    holds,
    invoke: (command: string, args?: { device?: string | null }): Promise<unknown> => {
      if (command === 'client_mic_hold') holds.push(args?.device ?? null);
      return Promise.resolve(undefined);
    },
    isTauri: (): boolean => true,
    listen: (): Promise<() => void> => Promise.resolve(() => {}),
  };
});

vi.mock('@tauri-apps/api/core', () => ({ invoke: shell.invoke, isTauri: shell.isTauri }));
vi.mock('@tauri-apps/api/event', () => ({ listen: shell.listen }));

import { installResizeObserver } from '../chat/testing/viewport';
import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION, type Subject } from '../protocol';
import type { Route } from '../routes';
import type { Connection, ConnectionStatus } from '../socket';
import type { Store, StoreValue } from '../stores';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { deviceFor, rememberDevice } from '../composer/dictation';
import { boxKey } from '../composer/box.svelte';
import Router from './Router.svelte';

installResizeObserver();

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
const KEY = boxKey(LEAD);

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  shell.holds.length = 0;
});

/** One store that has been subscribed and never answered, which draws empty. */
function emptyStore(what: Subject): Store {
  return {
    subject: what,
    value: writable<StoreValue>({
      snapshot: null,
      updates: [],
      state: { kind: 'loading' },
      dropped: 0,
    }),
    snapshot: () => null,
    updates: () => [],
    state: () => ({ kind: 'loading' }),
    dropped: () => 0,
    set: () => {},
    push: () => {},
    refuse: () => {},
  };
}

/**
 * A connection the session route can mount over: subscriptions answer with an
 * empty store (nothing about a seat is drawn here), and every call this test
 * is not about refuses loudly rather than passing quietly.
 */
function connection(): Connection {
  const refuse = (): never => {
    throw new Error('this mount reached a part of the connection it should not have');
  };
  const quiet = (): (() => void) => () => {};
  return {
    subscribe: (what: Subject) => emptyStore(what),
    unsubscribe: () => {},
    refresh: () => {},
    // The session page asks for its tail when it opens; a false says no page
    // is coming, so nothing waits on one.
    dispatch: () => null,
    more: () => false,
    frame: refuse,
    onBrowserAsk: refuse,
    browserRole: refuse,
    onBrowserRole: refuse,
    onProfilesChanged: quiet,
    takeBrowserRole: refuse,
    onMessage: quiet,
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      fn('open');
      return () => {};
    },
    store: (what: Subject) => emptyStore(what),
    settings: () => DEFAULT_SETTINGS,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open' as const,
    close: refuse,
  } as unknown as Connection;
}

/** The home wire with every dictation model ready, which is what offers the mic. */
function offered(): typeof homeWire {
  return {
    ...homeWire,
    dictate: {
      ...homeWire.dictate,
      snapshot: {
        ...homeWire.dictate.snapshot,
        models: homeWire.dictate.snapshot.models.map((model) => ({
          ...model,
          state: 'ready' as const,
        })),
      },
    },
  };
}

function draw(route: Route, wire = offered()): void {
  app = mount(Router, {
    target: document.body,
    props: {
      route,
      settings: DEFAULT_SETTINGS,
      address: '127.0.0.1:8790',
      home: { wire, refused: null, report: null },
      failure: null,
      connected: true,
      connection: connection(),
      notice: null,
      onconnect: () => {},
    },
  });
  flushSync();
}

describe('the microphone hold at the router', () => {
  it("opens the seat's pick as the page that takes typing is drawn", () => {
    rememberDevice(KEY, 'coreaudio:one');
    shell.holds.length = 0;

    draw({ name: 'session', slot: LEAD });

    expect(
      shell.holds,
      "the session page's first press must record from the press, not open a device under it",
    ).toEqual(['coreaudio:one']);
  });

  it('holds nothing while dictation is not offered', () => {
    const wire = offered();
    const pending = {
      ...wire,
      dictate: {
        ...wire.dictate,
        snapshot: {
          ...wire.dictate.snapshot,
          models: wire.dictate.snapshot.models.map((model) => ({
            ...model,
            state: 'pending' as const,
          })),
        },
      },
    } as typeof homeWire;

    draw({ name: 'session', slot: LEAD }, pending);

    expect(
      shell.holds,
      'a switched-off or not-yet-ready install must not light the indicator',
    ).toEqual([]);
  });

  it('holds nothing on a page that takes no typing', () => {
    draw({ name: 'home' });
    expect(shell.holds, 'the home draws no mic, so there is nothing to keep warm').toEqual([]);
  });

  it('follows the seat the page is on', () => {
    const worker: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'worker' };
    rememberDevice(boxKey(worker), 'coreaudio:two');
    shell.holds.length = 0;

    draw({ name: 'review', slot: worker });

    expect(shell.holds, "the review page takes typing too, on its own seat's pick").toEqual([
      'coreaudio:two',
    ]);
  });

  it('offers the default when the seat has no pick', () => {
    rememberDevice(KEY, null);
    shell.holds.length = 0;

    draw({ name: 'session', slot: LEAD });

    expect(shell.holds).toEqual([deviceFor(KEY)]);
  });
});

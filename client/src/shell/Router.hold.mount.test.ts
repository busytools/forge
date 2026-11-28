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

/** The shell, as the page reaches it: the holds it was asked for, and the events it raised. */
const shell = vi.hoisted(() => {
  const holds: (string | null)[] = [];
  const hidden: boolean[] = [];
  const hearing: { event: string; handler: (() => void) | null }[] = [];
  return {
    holds,
    hidden,
    hearing,
    invoke: (
      command: string,
      args?: { device?: string | null; hidden?: boolean },
    ): Promise<unknown> => {
      if (command === 'client_mic_hold') holds.push(args?.device ?? null);
      if (command === 'client_mic_hidden') hidden.push(args?.hidden === true);
      return Promise.resolve(undefined);
    },
    isTauri: (): boolean => true,
    listen: (event: string, handler: () => void): Promise<() => void> => {
      const entry: { event: string; handler: (() => void) | null } = { event, handler };
      hearing.push(entry);
      return Promise.resolve(() => {
        entry.handler = null;
      });
    },
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
  shell.hidden.length = 0;
  shell.hearing.length = 0;
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
      version: null,
      unexpected: [],
    }),
    snapshot: () => null,
    updates: () => [],
    state: () => ({ kind: 'loading' }),
    dropped: () => 0,
    version: () => null,
    unexpected: () => [],
    set: () => {},
    applied: () => {},
    gap: () => {},
    unplaced: () => {},
    push: () => {},
    compact: () => {},
    compacted: () => false,
    generation: () => 0,
    takeAudio: () => [],
    refuse: () => {},
  };
}

/**
 * A seat whose first conversation page has landed.
 *
 * The read wait holds the composer's body until the column reports the page
 * landed, and a take's controls are in that body - so a case about the mic
 * needs a seat that has answered, which is every seat after its first read.
 */
function landedStore(what: Subject): Store {
  return {
    ...emptyStore(what),
    snapshot: () => ({ conversation: { turns: [{ key: 't-1', messages: [] }], cursor: null } }),
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
    holds: () => true,
    refresh: () => {},
    showing: () => {},
    relist: () => {},
    declare: () => {},
    release: () => {},
    // The session page asks for its tail when it opens; a false says no page
    // is coming, so nothing waits on one.
    dispatch: () => null,
    more: () => false,
    frame: refuse,
    onMessage: quiet,
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      fn('open');
      return () => {};
    },
    store: (what: Subject) => landedStore(what),
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
      home: { wire, refused: null, failed: null, report: null },
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

  it('takes the hold up again when the shell says the screen is back', () => {
    rememberDevice(KEY, 'coreaudio:two');
    shell.holds.length = 0;

    draw({ name: 'session', slot: LEAD });
    expect(shell.holds, 'the hold is taken as the page draws').toEqual(['coreaudio:two']);

    const wake = shell.hearing.find((entry) => entry.event === 'client://mic-awake');
    expect(wake, 'the page listens for the shell saying the screen is back').toBeDefined();
    wake?.handler?.();
    expect(
      shell.holds,
      'the unlock re-holds without a press: a locked screen let the device go',
    ).toEqual(['coreaudio:two', 'coreaudio:two']);
  });

  it('lets the hold go while the page is hidden and takes it up when it is back', () => {
    rememberDevice(KEY, 'coreaudio:one');
    shell.holds.length = 0;
    shell.hidden.length = 0;

    draw({ name: 'session', slot: LEAD });
    expect(shell.holds, 'the hold is taken as the page draws').toEqual(['coreaudio:one']);

    const setHidden = (hidden: boolean): void => {
      Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
      document.dispatchEvent(new Event('visibilitychange'));
    };
    try {
      setHidden(true);
      expect(
        shell.hidden,
        'a hidden page tells the shell to let the hold go, or a minimized window holds a device it cannot use',
      ).toEqual([true]);
      expect(shell.holds, 'and takes no hold behind it').toEqual(['coreaudio:one']);

      setHidden(false);
      expect(shell.holds, 'the page coming back takes the hold up from its own pick').toEqual([
        'coreaudio:one',
        'coreaudio:one',
      ]);
    } finally {
      Reflect.deleteProperty(document, 'hidden');
    }
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

  it('draws the voice switch on [tts] alone, with dictation not offered', () => {
    // The switch is speech out only, so a dictation engine that is not ready
    // must not hide it: the mic is how the user speaks, and this control only
    // hears.
    const wire = offered();
    const noDictation = {
      ...wire,
      voice: { enabled: true, models_dir: null },
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

    draw({ name: 'session', slot: LEAD }, noDictation);

    expect(
      document.querySelector('.voice'),
      'the switch draws on [tts] alone, not on dictation being ready',
    ).not.toBeNull();
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

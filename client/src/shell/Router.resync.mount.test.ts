// @vitest-environment jsdom
/**
 * The return re-read through the real Router.
 *
 * **The watcher's only install site is an effect on this component**, and a
 * test at `resync.test.ts` cannot see whether anything calls it: deleting the
 * effect there leaves every test green and the feature gone. This mounts the
 * page, hides it, and comes back - one probe, one refresh of what the page
 * holds, and one reread of the seat on screen.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION, type ServerMessage, type Subject } from '../protocol';
import { sessionFrom } from '../session/wire';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import Router from './Router.svelte';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the return re-read, mounted', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  });

  afterEach(() => {
    vi.useRealTimers();
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
  });

  it('probes, refreshes what the page holds, and re-reads the shown seat', async () => {
    const stores = new Stores();
    const listeners = new Set<(message: ServerMessage) => void>();
    const quiet = (): (() => void) => () => {};
    const calls: string[] = [];
    const connection = {
      subscribe: (what: Subject) => stores.open(what),
      unsubscribe: (what: Subject) => stores.close(what),
      refresh: (what: Subject) => {
        calls.push(`refresh:${typeof what === 'string' ? what : 'seat'}`);
        return true;
      },
      relist: () => {},
      declare: () => {},
      release: () => {},
      showing: () => {},
      dispatch: () => Promise.resolve(null),
      more: () => false,
      commitDiff: () => false,
      fileContent: () => false,
      readDocument: () => false,
      seatRange: () => false,
      frame: () => false,
      browserInput: () => false,
      onBrowserFrame: quiet,
      onMessage: (fn: (message: ServerMessage) => void) => {
        listeners.add(fn);
        return () => {
          listeners.delete(fn);
        };
      },
      onStatus: (fn: (status: ConnectionStatus) => void) => {
        fn('open');
        return () => {};
      },
      store: (what: Subject) => stores.get(what),
      settings: () => DEFAULT_SETTINGS,
      skew: () => null,
      serverProtocol: () => PROTOCOL_VERSION,
      status: () => 'open' as const,
      probe: () => calls.push('probe'),
      close: () => {},
    } as unknown as Connection;

    app = mount(Router, {
      target: document.body,
      props: {
        route: { name: 'session', slot: LEAD },
        settings: DEFAULT_SETTINGS,
        address: '127.0.0.1:8790',
        home: { wire: homeWire, refused: null, failed: null, report: null },
        failure: null,
        connected: true,
        connection,
        notice: null,
        onconnect: () => {},
      },
    });
    flushSync();

    // The seat's own read answers, so the page's next ask is a real one.
    const seeded = sessionFrom({ slot: LEAD, state: { scan_cwd: '/tmp' } });
    stores.open({ session: LEAD }).set(seeded);
    for (const fn of listeners) {
      fn({ kind: 'snapshot', subject: { session: LEAD }, data: seeded });
    }
    flushSync();
    calls.length = 0;

    hidden = true;
    document.dispatchEvent(new Event('visibilitychange'));
    await vi.advanceTimersByTimeAsync(3_000);
    hidden = false;
    document.dispatchEvent(new Event('visibilitychange'));
    flushSync();

    expect(calls, 'one probe first, then the board and the shown seat').toEqual([
      'probe',
      'refresh:board',
      'refresh:seat',
    ]);
  });
});

/** Whether the document reports itself hidden, as the browser would. */
let hidden = false;

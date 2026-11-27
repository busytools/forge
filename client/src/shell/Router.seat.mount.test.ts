// @vitest-environment jsdom
/**
 * The seat's held patches and catching-up state, through the real Router.
 *
 * **Only the store knows these two, and its notifications are the only
 * carrier**: a patch the socket could not place and a version gap write no
 * frame, so a page that repaints only on frames and answers drew neither -
 * and the whole read a gap asks for clears both before the page ever says so.
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION, subjectKey, type ServerMessage, type Subject } from '../protocol';
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

it('draws the patches the seat could not place, and says it is catching up', () => {
  const stores = new Stores();
  /** The subjects the page has actually subscribed: a store alone is not one. */
  const wants = new Set<string>();
  const listeners = new Set<(message: ServerMessage) => void>();
  const quiet = (): (() => void) => () => {};
  const connection = {
    subscribe: (what: Subject) => {
      wants.add(subjectKey(what));
      return stores.open(what);
    },
    unsubscribe: (what: Subject) => {
      wants.delete(subjectKey(what));
      return stores.close(what);
    },
    holds: (what: Subject) => wants.has(subjectKey(what)),
    refresh: () => true,
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
    probe: () => {},
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

  // The seat's first read answers, so the page has a record to repaint.
  const seeded = sessionFrom({ slot: LEAD, state: { scan_cwd: '/tmp' } });
  stores.open({ session: LEAD }).set(seeded);
  for (const fn of listeners) {
    fn({ kind: 'snapshot', subject: { session: LEAD }, data: seeded });
  }
  flushSync();

  // The store's own two facts move, as the socket's gap and its held patch do.
  const store = stores.get({ session: LEAD });
  store?.gap('a skipped version');
  store?.unplaced({ state: 'unreadable' });
  flushSync();

  const body = document.body.textContent ?? '';
  expect(body, 'the catching-up state was not drawn').toContain('Catching up with the server');
  expect(body, 'the held patch was not drawn').toContain('for the seat could not be read and');
});

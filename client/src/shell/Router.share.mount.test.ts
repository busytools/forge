// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { PROTOCOL_VERSION } from '../protocol';
import { currentRoute } from '../routes';
import { pendingShare } from '../share/pending';
import type { Connection } from '../socket';
import { Stores } from '../stores';
import { DEFAULT_SETTINGS } from '../wire/types';
import Router from './Router.svelte';

/**
 * **A second share is a new sheet.** The sheet is keyed on the share, so a
 * share arriving while the first one's "Sent to" state is still on screen
 * remounts it idle: without the key, pressing Send on the second share would
 * dispatch the FIRST share's paths and never upload the new files.
 */

// The upload the sheet drives: one answer, so the first share reaches its
// done state and the second one's arrival is what the test observes.
vi.mock('../share/upload', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../share/upload')>();
  return { ...actual, uploadShare: vi.fn(() => Promise.resolve({ paths: ['/inbox/first.jpg'] })) };
});

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  pendingShare.set(null);
  currentRoute.set({ name: 'home' });
});

/** A connection the share page reaches only for its subscribe and a prompt. */
function connection(): Connection {
  const stores = new Stores();
  const refuse = (): never => {
    throw new Error('the share route reached a part of the connection it should not have');
  };
  return {
    subscribe: (what: unknown) => stores.open(what as never),
    unsubscribe: () => {},
    refresh: () => true,
    showing: () => {},
    relist: () => {},
    release: () => {},
    dispatch: () => Promise.resolve(null),
    more: refuse,
    onMessage: () => () => {},
    onStatus: () => () => {},
    store: refuse,
    settings: () => null,
    skew: refuse,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    close: refuse,
  } as unknown as Connection;
}

function draw(): void {
  app = mount(Router, {
    target: document.body,
    props: {
      route: { name: 'share' },
      settings: DEFAULT_SETTINGS,
      address: '127.0.0.1:8790',
      home: { wire: homeWire, refused: null, failed: null, report: null },
      failure: null,
      connected: true,
      connection: connection(),
      notice: null,
      onconnect: () => {},
    },
  });
  flushSync();
}

describe('a second share through the router', () => {
  it('resets the sheet instead of keeping the first share sent', async () => {
    pendingShare.set({
      files: [{ path: '/cache/1/a.jpg', name: 'a.jpg', size: 1, mime: null }],
      error: null,
    });
    draw();
    expect(document.body.textContent).toContain('a.jpg');

    const send = [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (button) => button.textContent?.trim() === 'send',
    );
    if (send === undefined) throw new Error('the send control is not on the page');
    send.click();
    await vi.waitFor(() => {
      flushSync();
      expect(document.body.textContent).toContain('Sent to');
    });

    pendingShare.set({
      files: [{ path: '/cache/2/b.pdf', name: 'b.pdf', size: 2, mime: null }],
      error: null,
    });
    flushSync();
    expect(document.body.textContent).toContain('b.pdf');
    expect(document.body.textContent).not.toContain('Sent to');
    expect(document.body.textContent).not.toContain('a.jpg');
  });
});

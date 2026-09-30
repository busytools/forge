import { render } from 'svelte/server';
import { describe, expect, it } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Router from './Router.svelte';

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * A connection the server render never reaches.
 *
 * The session page subscribes in an effect, and `render` from `svelte/server`
 * runs no effects - so this is here to satisfy the prop. Every method throws
 * rather than answering quietly, so a render that did start reaching it fails
 * loudly instead of drawing a page built on a connection that is not there.
 */
function untouched(): Connection {
  const refuse = (): never => {
    throw new Error('the server render reached the connection');
  };
  return {
    subscribe: refuse,
    unsubscribe: refuse,
    refresh: refuse,
    dispatch: refuse,
    more: refuse,
    onMessage: refuse,
    onStatus: refuse,
    store: refuse,
    settings: refuse,
    status: refuse,
    close: refuse,
  };
}

/**
 * A session address, drawn.
 *
 * **The two columns on this page are the router's to mount**, because the
 * session page takes them as snippets rather than importing them: it stands on
 * its own until they land, and a page that mounted neither draws a header and
 * an empty column with nothing to say it is broken. That is exactly what a
 * session address drew while the conversation was never handed over.
 */
function draw(): string {
  return render(Router, {
    props: {
      route: { name: 'session', slot: LEAD },
      settings: { mark: null, theme: null, font: null },
      address: '127.0.0.1:8790',
      home: { wire: homeWire, refused: null },
      failure: null,
      connected: true,
      connection: untouched(),
      onconnect: () => {},
    },
  }).body;
}

describe('the router at a session address', () => {
  /**
   * The conversation column is where the seat's history draws, and it is the
   * one thing on this page a reader opens a session for. A router that hands
   * over only the composer leaves the column empty with no state at all: not
   * loading, not empty, not refused.
   */
  it('mounts the conversation column', () => {
    expect(draw(), 'the session page drew no conversation column').toContain(
      'Reading the conversation',
    );
  });
});

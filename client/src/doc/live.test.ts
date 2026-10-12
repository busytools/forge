import { get, type Readable } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { watchDocument, type DocumentConnection } from './live';

/**
 * The watchers are lazy stores: their start runs on the FIRST subscriber,
 * so a test that only calls `watchDocument` has asked the socket nothing.
 * Every watcher here is held for the test's life.
 */
const stops: (() => void)[] = [];
afterEach(() => {
  for (const stop of stops.splice(0)) stop();
});

function hold<T>(view: Readable<T>): Readable<T> {
  stops.push(view.subscribe(() => {}));
  return view;
}

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const OTHER: SessionSlot = { org: 'Busytools', project: 'forge', label: 'w1' };

/** A socket that records what was asked and can be made to speak. */
function fakeSocket() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  const reads: { slot: SessionSlot; path: string; reviewable: boolean }[] = [];
  let open = true;
  const connection = {
    readDocument(slot: SessionSlot, path: string, reviewable: boolean): boolean {
      if (!open) return false;
      reads.push({ slot, path, reviewable });
      return true;
    },
    onMessage(fn: (message: ServerMessage) => void) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus(fn: (status: ConnectionStatus) => void) {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    status: (): ConnectionStatus => (open ? 'open' : 'closed'),
  } as unknown as DocumentConnection;
  return {
    connection,
    reads,
    emit(message: ServerMessage) {
      for (const fn of [...listeners]) fn(message);
    },
    drop() {
      open = false;
      for (const fn of [...statuses]) fn('closed');
    },
    reopen() {
      open = true;
      for (const fn of [...statuses]) fn('open');
    },
  };
}

describe('the document read', () => {
  it('asks the server for the document when the route opens', () => {
    const socket = fakeSocket();
    const view = hold(watchDocument(socket.connection, LEAD, '/proj/docs/plan.md', true));

    const held = get(view);

    expect(socket.reads).toEqual([{ slot: LEAD, path: '/proj/docs/plan.md', reviewable: true }]);
    expect(held.state, 'nothing has landed yet').toBe('reading');
  });

  it('draws the answer that names its seat and its document', () => {
    const socket = fakeSocket();
    const view = hold(watchDocument(socket.connection, LEAD, '/proj/docs/plan.md', true));

    socket.emit({
      kind: 'document_read',
      session: LEAD,
      path: '/proj/docs/plan.md',
      document_kind: 'markdown',
      bytes: btoa('# hi\n'),
      reviewable: true,
      content_hash: '42',
      threads: [
        {
          id: 'd1',
          anchor: {
            path: '/proj/docs/plan.md',
            side: 'New',
            line: 3,
            content_hash: '42',
            context: ['context'],
            base_ref: 'doc:/proj/docs/plan.md',
          },
          comments: [{ author: 'User', text: 'hello', at: 'x', review_id: null }],
          status: 'Open',
          created_at: 'x',
          updated_at: 'x',
          commit: null,
        },
      ],
      reviews: [{ id: 'r1', number: 1, summary: null, created_at: 'x' }],
    });

    const held = get(view);
    expect(held.state).toBe('landed');
    expect(held.kind).toBe('markdown');
    expect(new TextDecoder().decode(held.bytes ?? new Uint8Array())).toBe('# hi\n');
    expect(held.reviewable).toBe(true);
    expect(held.contentHash, 'the document hash crosses as the digits it arrived as').toBe('42');
    expect(
      held.threads.map((thread) => thread.id),
      'its comments narrow with it',
    ).toEqual(['d1']);
    expect(held.threads[0]?.anchor.base_ref).toBe('doc:/proj/docs/plan.md');
    expect(held.reviews.map((set) => set.number)).toEqual([1]);
  });

  /**
   * **The seat and the path are both the match.** A client holding two
   * seats can have two viewers' reads in flight, and an answer that only
   * matched on the path would draw one seat's document in the other's page.
   */
  it('ignores an answer for another seat or another document', () => {
    const socket = fakeSocket();
    const view = hold(watchDocument(socket.connection, LEAD, '/proj/docs/plan.md', false));

    socket.emit({
      kind: 'document_read',
      session: OTHER,
      path: '/proj/docs/plan.md',
      document_kind: 'text',
      bytes: btoa('other seat'),
      reviewable: false,
      content_hash: '1',
      threads: [],
      reviews: [],
    });
    socket.emit({
      kind: 'document_read',
      session: LEAD,
      path: '/proj/docs/other.md',
      document_kind: 'text',
      bytes: btoa('other doc'),
      reviewable: false,
      content_hash: '2',
      threads: [],
      reviews: [],
    });

    expect(get(view).state, "neither answer is this page's").toBe('reading');
  });

  it("draws the server's refusal in its own words", () => {
    const socket = fakeSocket();
    const view = hold(watchDocument(socket.connection, LEAD, 'gone.md', false));

    socket.emit({
      kind: 'error',
      what: 'document_read',
      why: 'no document at gone.md',
      seat: LEAD,
    });

    const held = get(view);
    expect(held.state).toBe('refused');
    expect(held.refused).toBe('no document at gone.md');
  });

  /** A read that could not even be sent is the page's own refusal, not a
   * silence that reads as still loading. */
  it('refuses its own ask when the socket is down', () => {
    const socket = fakeSocket();
    socket.drop();

    const held = get(watchDocument(socket.connection, LEAD, '/proj/docs/plan.md', false));

    expect(held.state).toBe('refused');
    expect(held.refused).toContain('connection');
    expect(socket.reads, 'nothing went out').toEqual([]);
  });

  /** A drop mid-read takes the answer with it; the reopen asks again rather
   * than leaving the page on "opening..." for good. */
  it('asks again after a drop that killed the read', () => {
    const socket = fakeSocket();
    hold(watchDocument(socket.connection, LEAD, '/proj/docs/plan.md', false));
    socket.drop();

    socket.reopen();

    expect(socket.reads).toHaveLength(2);
  });
});

// @vitest-environment jsdom
/**
 * What the column hands the virtualiser to bound its window.
 *
 * **The list is a stub, because the bound is a mount fact.** `virtua` re-derives
 * its own estimate from the rows it has measured unless the column pins one,
 * and jsdom lays nothing out - so what has to be seen is the seed the column
 * passes, not a measurement of a window no browser here can draw (#1909, #1555).
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';

vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

const { default: Chat } = await import('./Chat.svelte');
const { installResizeObserver } = await import('./testing/viewport');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** A connection the test drives by hand, answering only what a mount asks. */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: () => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (_fn: (status: ConnectionStatus) => void) => () => undefined,
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;
  return {
    connection,
    page(turns: unknown[]): void {
      for (const fn of listeners) {
        fn({ kind: 'page', conversation: LEAD, turns, cursor: null });
      }
      flushSync();
    },
  };
}

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the window the chat hands the virtualiser', () => {
  it('seeds an unmeasured row with a fixed height of its own', () => {
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([
      {
        key: 't1',
        messages: [
          {
            type: 'user',
            uuid: 'u-1',
            message: { role: 'user', content: [{ type: 'text', text: 'a row' }] },
          },
        ],
      },
    ]);

    const seed = Number(document.querySelector('.conv')?.getAttribute('data-item-size'));
    // The constant is the contract: leaving it unset restores virtua's own
    // running median, and any other value is a deliberate re-estimate of a
    // short turn's height.
    expect(seed, 'the list is handed the fixed seed rather than left to its own median').toBe(120);
  });
});

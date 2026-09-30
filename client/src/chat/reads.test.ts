// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * The list is a stub, as it is for the scroll tests: what has to be seen here
 * is what the column HANDS the list as the page re-reads, and `virtua` measures
 * through APIs jsdom does not implement.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

const { default: Churned } = await import('./testing/Churned.svelte');

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** The connection the column reaches for, and the asks it makes of it. */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asks: ClientMessage[] = [];
  const connection = {
    more: (conversation: SessionSlot, before: string | null, turns: number) => {
      asks.push({ kind: 'more', conversation, before, turns });
      return true;
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
  } as unknown as Connection;

  return {
    connection,
    asks,
    /** The page the server would answer `more` with. */
    answer(turns: unknown[]): void {
      for (const fn of listeners) fn({ kind: 'page', conversation: LEAD, turns, cursor: null });
      flushSync();
    },
  };
}

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

/**
 * The column under the page it really draws in.
 *
 * **The page re-derives what it hands the column on every read it makes**, and
 * a seat re-reads on every frame it emits while its words arrive. Re-writing an
 * object-valued prop counts as a change however equal it is, so a column whose
 * conversation is keyed on its props is torn down and rebuilt many times a
 * second - which a reader sees as the column flickering between their
 * conversation and its own loading state.
 */
describe('the chat column under a page that re-reads', () => {
  it('keeps the conversation it built when the page hands its props over again', () => {
    const server = stub();
    const reads = writable(0);
    app = mount(Churned, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection, reads },
    });
    flushSync();
    server.answer([{ key: 't1', messages: [{ type: 'assistant', uuid: 'a1' }] }]);
    const drawn = (): string => document.body.textContent ?? '';
    expect(drawn(), 'the page answered and the column did not draw it').not.toContain(
      'Reading the conversation',
    );

    reads.set(1);
    flushSync();
    reads.set(2);
    flushSync();

    expect(server.asks, "the page's own read asked the server for the conversation again").toEqual([
      { kind: 'more', conversation: LEAD, before: null, turns: 20 },
    ]);
    expect(drawn(), "the page's own read put the column back to loading").not.toContain(
      'Reading the conversation',
    );
  });

  it("draws what a Read came back with as the page's own code panel", () => {
    const server = stub();
    app = mount(Churned, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection, reads: writable(0) },
    });
    flushSync();
    server.answer([
      {
        key: 't1',
        messages: [
          {
            type: 'assistant',
            uuid: 'a1',
            message: {
              role: 'assistant',
              model: 'claude-opus-5',
              content: [
                {
                  type: 'tool_use',
                  id: 'c1',
                  name: 'Read',
                  input: { file_path: 'crates/forge-server/src/family.rs' },
                },
              ],
            },
          },
          {
            type: 'user',
            uuid: 'r1',
            message: {
              role: 'user',
              content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'pub fn main() {}' }],
            },
          },
        ],
      },
    ]);

    // The panel the fence in a message also draws, so a slip in the builder
    // leaves both surfaces wrong rather than one.
    expect(document.body.innerHTML, 'the read body is the code panel').toContain(
      '<div class="code">',
    );
    expect(
      document.body.innerHTML,
      'and the header names the language the call path names',
    ).toContain('<div class="lang">rust</div>');
  });
});

// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Chat from './Chat.svelte';

/**
 * The list is a stub, as it is for the scroll tests: what has to be seen here
 * is what the column hands the rows, and `virtua` measures through APIs jsdom
 * does not implement.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/**
 * A connection a test drives by hand.
 *
 * The chat reaches it for two things - the page it asks for, and the frames it
 * is sent - so those are what this answers with. It is not a socket test:
 * `socket.test.ts` covers the wire, against a real stub server.
 */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asks: ClientMessage[] = [];
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'loading' }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (conversation: SessionSlot, before: string | null, turns: number) => {
      asks.push({ kind: 'more', conversation, before, turns });
      return true;
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;

  return {
    connection,
    asks,
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
      flushSync();
    },
    /** The page the server would answer `more` with. */
    answer(turns: unknown[], cursor: string | null = null): void {
      this.send({ kind: 'page', conversation: LEAD, turns, cursor });
    },
  };
}

let app: Record<string, unknown> | null = null;

function draw(props: Record<string, unknown>, server: ReturnType<typeof stub>): void {
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection, cwd: null, ...props },
  });
  flushSync();
}

/** What the column reads as, which is what a reader has to go on. */
const drawn = (): string => document.body.textContent ?? '';

afterEach(async () => {
  // One page per test: the column is read off the document, so a mount left
  // behind is read as part of the next test's page.
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the chat column as it draws', () => {
  it('asks for the newest page before it draws anything', () => {
    const server = stub();
    draw({}, server);

    // No cursor on the first ask: a cursor asks for what is above a row the
    // reader already has, and asking with one before anything is held is how
    // the page opens at the top of a conversation instead of the end.
    expect(server.asks).toEqual([{ kind: 'more', conversation: LEAD, before: null, turns: 20 }]);
  });

  it('says a seat has no history rather than drawing a blank column', () => {
    const server = stub();
    draw({}, server);
    server.answer([]);

    expect(drawn()).toContain('Nothing said yet');
    expect(drawn()).toContain('no history');
  });

  it('says it is still reading rather than saying the seat is empty', () => {
    const server = stub();
    draw({}, server);

    // A page that has not answered yet is not an empty conversation: saying
    // "nothing said yet" here tells the reader the seat is new when the truth
    // is that nothing has come back.
    expect(drawn()).toContain('Reading the conversation');
  });

  it('hands back the words the server turned the conversation down with', () => {
    const server = stub();
    draw({}, server);
    server.send({ kind: 'error', what: 'more', why: 'forge holds no session for that seat' });

    expect(drawn()).toContain('forge holds no session for that seat');
  });

  it('draws the compaction line once, under the newest turn only', () => {
    // The prop is the conversation's, and the line is the newest turn's: a
    // column that handed it to every turn would draw a line per row, which is
    // one line per turn in the reader's history.
    const said = (text: string): unknown => ({
      type: 'assistant',
      message: {
        id: `m-${text}`,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text }],
      },
    });

    const server = stub();
    draw({ compacting: true }, server);
    server.answer([
      { key: 't1', messages: [said('the first answer')] },
      { key: 't2', messages: [said('the second answer')] },
    ]);
    flushSync();

    const lines = (document.body.textContent ?? '').match(/Compacting context/g) ?? [];
    expect(lines, 'one line for the conversation, not one per turn').toHaveLength(1);
  });

  it('draws the seat that has no session behind it as its own state', () => {
    const server = stub();
    draw({ waking: true, reason: 'no model declared' }, server);

    expect(drawn()).toContain('not running');
    expect(drawn()).toContain('no model declared');
  });
});

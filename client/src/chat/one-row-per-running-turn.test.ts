import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import type { ServerMessage, SessionUpdate } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat, type PageTurn } from './conversation';
import { fold, type Unit } from './units';

/**
 * One row per running turn, carried from open to settle.
 *
 * The row has to draw the bar (the ring, the elapsed clock, `thinking N`),
 * and nothing arriving mid-turn may split it in two. Two cases below are
 * known defects on this branch and carry `it.fails` rather than `it`: the
 * turn's liveness is set only by the frames path, so a running turn the
 * client learned from its PAGE draws no bar, and the next drawing frame then
 * opens a second row beside it. Both flip to plain `it` when the newest page
 * row is treated as live while the record says `header.turn_in_flight` - the
 * core's own answer for a turn in flight, already in the record a page holds.
 */

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One assistant message carrying prose. */
const said = (text: string): unknown => ({
  type: 'assistant',
  uuid: `a-${text}`,
  timestamp: '2026-10-01T10:00:00Z',
  message: {
    id: `m-${text}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
    usage: {
      input_tokens: 100,
      output_tokens: 10,
      cache_read_input_tokens: 1000,
      cache_creation_input_tokens: 0,
    },
  },
});

/** The reader's own words, as the CLI persisted them. */
const typed = (text: string): unknown => ({
  type: 'user',
  uuid: `u-${text}`,
  timestamp: '2026-10-01T09:59:00Z',
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/** One `system/thinking_tokens` frame, about every fifty tokens. */
const counted = (delta: number): unknown => ({
  type: 'system',
  subtype: 'thinking_tokens',
  estimated_tokens: delta,
  estimated_tokens_delta: delta,
  uuid: `thinking-${delta}`,
  timestamp: '2026-10-01T10:00:01Z',
});

/**
 * A connection a test drives by hand, as `conversation.test.ts` has one: what
 * the chat needs is what it asked for, the frames it was sent, and the store
 * it subscribed to.
 */
function fakeConnection() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();

  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'loading' }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (_conversation: SessionSlot, _before: string | null, _turns: number) => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;

  return {
    connection,
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
    },
    update(update: SessionUpdate): void {
      this.send({ kind: 'update', update });
    },
  };
}

const page = (turns: PageTurn[], cursor: string | null): ServerMessage => ({
  kind: 'page',
  conversation: LEAD,
  turns,
  cursor,
});

/** Whether the units carry the bar: a running report row. */
const running = (units: Unit[]): boolean =>
  units.some((unit) => unit.kind === 'report' && unit.info.running);

describe('one row per running turn', () => {
  it('draws the bar for a turn the frames opened', () => {
    // The order the wire's own rule gives: with no turn to join, the first
    // frame the fold draws something out of opens one, and the counters land
    // inside it.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine')] }], null));

    server.update({ chat_appended: { key: LEAD, msg: said('working') } });
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });

    const held = get(chat.value).turns.at(-1);
    expect(held?.live, 'the turn the frames opened is live').toBe(true);
    expect(running(fold(held?.messages ?? [], null, null, held?.live ?? false)), 'the bar').toBe(
      true,
    );
  });

  it.fails('draws the bar for a running turn the client learned from a page', () => {
    // What a page open mid-turn really sees: the newest page row IS the turn
    // being written, and a page read while a turn runs carries no result frame
    // for it. The counters then arrive - a `system` frame about every fifty
    // tokens, all of them drawing nothing on their own.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });

    const held = get(chat.value).turns.at(-1);
    expect(held?.live, 'the running turn the page carried is live').toBe(true);
    expect(running(fold(held?.messages ?? [], null, null, held?.live ?? false)), 'the bar').toBe(
      true,
    );
  });

  it.fails('holds a running turn the page carried in one row, with its bar', () => {
    // The other end of the case above: when the thought lands, the drawing
    // frame is the first one that can open a turn, and with the row above it
    // not live it opens a second - one turn in two rows, the bar on the tail.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });
    server.update({ chat_appended: { key: LEAD, msg: said('the thought lands') } });
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });

    const turns = get(chat.value).turns;
    const rows = turns.map((row) => ({
      key: row.key,
      live: row.live,
      messages: row.messages.length,
      bar: running(fold(row.messages, null, null, row.live)),
    }));
    const holding = turns.filter((row) => row.key === 't1' || row.live).length;
    expect(
      holding,
      `the running turn is held once, and it has the bar; rows: ${JSON.stringify(rows)}`,
    ).toBe(1);
  });
});

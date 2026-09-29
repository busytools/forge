import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import { MORE_TURNS } from '../protocol';
import type { ClientMessage, ServerMessage, SessionUpdate } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat, type PageTurn } from './conversation';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One turn as a page carries it. */
const turn = (key: string | null, ...texts: string[]): PageTurn => ({
  key,
  messages: texts.map((text) => ({
    type: 'user',
    message: { role: 'user', content: [{ type: 'text', text }] },
  })),
});

/** One assistant message carrying prose. */
const said = (text: string): unknown => ({
  type: 'assistant',
  message: {
    id: `m-${text}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
  },
});

/** A page of whole turns, as the server answers `more`. */
const page = (turns: PageTurn[], cursor: string | null): ServerMessage => ({
  kind: 'page',
  conversation: LEAD,
  turns,
  cursor,
});

/**
 * A connection a test drives by hand.
 *
 * The real one is `socket.ts`, whose own tests cover the wire. What the chat
 * needs from it is three things - what it asked for, the frames it was sent,
 * and the store it subscribed to - so that is what this answers with.
 */
function fakeConnection() {
  const asks: ClientMessage[] = [];
  const listeners = new Set<(message: ServerMessage) => void>();
  const updates: SessionUpdate[] = [];

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
    /** The `more` asks alone, which is what every test here reads. */
    more: () => asks.filter((ask) => ask.kind === 'more'),
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
    },
    /** One session update for this seat, as the socket delivers it. */
    update(update: SessionUpdate): void {
      updates.push(update);
      this.send({ kind: 'update', update });
    },
  };
}

describe('the conversation the chat draws', () => {
  it('opens at the latest turn rather than the first', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // The newest page is the one with no cursor: `null` asks for what is at
    // the end, and a cursor asks for what is above a row the reader already
    // has. Asking with a cursor on the first ask is how the page opens at the
    // top - the defect the gate rejected the last batch for.
    expect(server.more()).toEqual([
      { kind: 'more', conversation: LEAD, before: null, turns: MORE_TURNS },
    ]);

    server.send(page([turn('t1', 'first'), turn('t2', 'second')], '3'));

    const read = get(chat.value);
    expect(read.loaded).toBe(true);
    expect(read.turns.map((held) => held.key)).toEqual(['t1', 't2']);
    expect(read.cursor, 'and the page carries the handle for the turns above it').toBe('3');
  });

  it('does not move the reader when an update arrives while they are scrolled up', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first'), turn('t2', 'second')], null));

    // The reader scrolls up: their place is a distance from the newest end,
    // and every turn below them may grow without it changing.
    chat.position(false);
    const before = get(chat.value);

    server.update({ chat_appended: { key: LEAD, msg: said('a line arriving') } });

    const after = get(chat.value);
    expect(after.atEnd, 'the reader is still where they were, not pulled to the end').toBe(false);
    expect(
      after.turns.slice(0, before.turns.length - 1),
      'and nothing above the turn the frame belongs to moved',
    ).toEqual(before.turns.slice(0, before.turns.length - 1));
  });

  it('re-renders only the turn in flight when its frames arrive', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first'), turn('t2', 'second')], null));
    const before = get(chat.value).turns;

    server.update({ chat_appended: { key: LEAD, msg: said('a line arriving') } });

    const after = get(chat.value).turns;
    // One turn is rebuilt and the rest are the very objects they were, which
    // is what keeps the virtualiser from re-measuring a row the reader is not
    // looking at. A client that mapped every turn into a fresh object on each
    // frame would re-render the whole conversation to grow one row.
    expect(after[0], 'the first turn is the object it was').toBe(before[0]);
    expect(after[1], 'and so is the one the frame did not touch').toBe(before[1]);
    expect(after[2], 'the frame opened a turn of its own').toBeDefined();
  });

  it('keeps every row keyed when older turns arrive', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t2', 'second'), turn('t3', 'third')], '2'));
    const held = get(chat.value).turns.map((row) => row.key);

    chat.older();
    expect(server.more()[1], 'the second ask echoes the handle the first page carried').toEqual({
      kind: 'more',
      conversation: LEAD,
      before: '2',
      turns: MORE_TURNS,
    });
    server.send(page([turn('t0', 'first'), turn('t2', 'second')], null));

    const rows = get(chat.value).turns;
    // A page may repeat a row the client already holds - the server errs
    // toward repeating rather than toward a gap - so the repeats are dropped
    // and the ones already drawn keep the name they had.
    expect(rows.map((row) => row.key)).toEqual(['t0', 't2', 't3']);
    expect(
      rows.slice(1).map((row) => row.key),
      'every key survived the prepend',
    ).toEqual(held);
    expect(get(chat.value).cursor, 'and a null cursor is the end of the walk').toBeNull();
  });

  it('draws its shell and says so when a seat has no history', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([], null));

    const read = get(chat.value);
    expect(read.loaded, 'an empty conversation is a state, not a page still loading').toBe(true);
    expect(read.turns).toEqual([]);
    expect(read.refused, 'and nothing was refused').toBeNull();
  });

  it('names a turn the fold did not, and does not rename it when one is prepended', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn(null, 'the fold named this one nothing')], '1'));

    const [named] = get(chat.value).turns;
    expect(named, 'the turn is held').toBeDefined();
    // A key taken from the turn's POSITION would survive this by accident and
    // break on the next prepend, which is the defect: the row above the
    // reader's place is the one whose name must not move.
    expect(named?.key, 'a turn the fold could not name is named here').toBeTruthy();

    chat.older();
    server.send(page([turn('t0', 'older')], null));

    const rows = get(chat.value).turns;
    expect(
      rows.map((row) => row.key),
      'the older turn went above it',
    ).toEqual(['t0', named?.key]);
  });

  it('does not lose the walk back when a turn settles', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t2', 'second')], '2'));
    // The reader walks a page back, so the handle they hold names a place
    // well above the newest page.
    chat.older();
    server.send(page([turn('t1', 'first')], '1'));

    // A turn settles and the chat asks for the newest page again. The handle
    // it then holds must still be the reader's place in the walk: taking the
    // fresh page's own handle instead sends the walk back to the top of the
    // conversation, and every page between is fetched a second time.
    chat.refresh();
    expect(get(chat.value).cursor, 'the walk is where it was').toBe('1');
    expect(server.more()[2]).toEqual({
      kind: 'more',
      conversation: LEAD,
      before: null,
      turns: MORE_TURNS,
    });
    server.send(page([turn('t2', 'second'), turn('t3', 'third')], '2'));

    expect(get(chat.value).cursor, 'and it is still where it was').toBe('1');
    expect(get(chat.value).turns.map((row) => row.key)).toEqual(['t1', 't2', 't3']);
  });

  it('draws a repeated turn once even when the fold gave it no name', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn(null, 'the fold named this one nothing')], null));
    const held = get(chat.value).turns;

    // What a settled turn does is ask for the newest page again, and the
    // server errs toward repeating a row rather than toward a gap - so the
    // page it answers with holds the turn already drawn. A repeat is dropped
    // by the name the conversation gave the turn, which for an unnamed one is
    // its own content: matching only the fold's own name makes every unnamed
    // turn a stranger on the way back in, and the column draws it twice.
    chat.refresh();
    server.send(page([turn(null, 'the fold named this one nothing')], null));

    expect(get(chat.value).turns).toHaveLength(1);
    expect(get(chat.value).turns[0], 'and it is the object the reader is looking at').toBe(held[0]);
  });

  it('takes a page it asked for as its own, and leaves another message alone', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // The socket hands a listener every message the server sends, so a page
    // that took any error as its own would draw a refused subscription, or a
    // refused command, as a conversation this forge will not answer for.
    server.send({ kind: 'error', what: 'dispatch', why: 'the socket is not open' });

    expect(get(chat.value).refused).toBeNull();
    expect(get(chat.value).loaded).toBe(true);
  });

  it('holds the reader when the socket drops, and takes the new page when it returns', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));
    const held = get(chat.value).turns;

    // A drop takes the connection's stores with it and the reconnect answers
    // with a snapshot. The chat's turns are its own, so a page arriving from
    // the fresh subscription is merged into them the same way an older page
    // is - and the reader does not watch the conversation empty and refill.
    server.send(page([turn('t1', 'first')], null));

    expect(get(chat.value).turns.map((row) => row.key)).toEqual(['t1']);
    expect(get(chat.value).turns[0]).toBe(held[0]);
  });
});

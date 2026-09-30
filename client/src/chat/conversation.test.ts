import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import { MORE_TURNS } from '../protocol';
import type { ClientMessage, ServerMessage, SessionUpdate } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat, type PageTurn } from './conversation';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/**
 * One turn as a page carries it.
 *
 * Each message carries a `uuid`, which is what the wire sends and what this
 * client names a turn the fold could not name by.
 */
const turn = (key: string | null, ...texts: string[]): PageTurn => ({
  key,
  messages: texts.map((text) => ({
    type: 'user',
    uuid: `u-${text}`,
    message: { role: 'user', content: [{ type: 'text', text }] },
  })),
});

/** One assistant message carrying prose. */
const said = (text: string): unknown => ({
  type: 'assistant',
  uuid: `a-${text}`,
  message: {
    id: `m-${text}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text }],
  },
});

/** The frame a turn ends on, which is what tells a page's copy of it settled. */
const ended = (): unknown => ({
  type: 'result',
  uuid: 'r-1',
  subtype: 'success',
  is_error: false,
});

/** A page of whole turns, as the server answers `more`. */
const page = (turns: PageTurn[], cursor: string | null): ServerMessage => ({
  kind: 'page',
  conversation: LEAD,
  turns,
  cursor,
});

/** One `system` frame: the thinking-token counter, which draws nothing. */
const counted = (tokens: number): unknown => ({
  type: 'system',
  subtype: 'thinking_tokens',
  estimated_tokens: tokens,
  estimated_tokens_delta: tokens,
  uuid: `thinking-${tokens}`,
});

/** Another `system` subtype, which the fold draws nothing for either. */
const progressed = (): unknown => ({
  type: 'system',
  subtype: 'task_progress',
  task_id: 'task-1',
  uuid: 'progress-1',
});

/** A `user` frame carrying only a tool result, which the fold draws nothing for. */
const result = (id: string): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: 'ok' }] },
});

/** An `assistant` frame carrying only thinking, which the fold draws nothing for. */
const thought = (): unknown => ({
  type: 'assistant',
  message: {
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'thinking', thinking: '...' }],
  },
});

/** An `assistant` frame whose only call is a monitor, which the fold skips. */
const monitoring = (): unknown => ({
  type: 'assistant',
  message: {
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'tool_use', id: 'mon-1', name: 'Monitor', input: {} }],
  },
});

/** A dispatched agent's frame, whose words belong to the SUBAGENTS surface. */
const dispatched = (): unknown => ({
  type: 'assistant',
  parent_tool_use_id: 'call-9',
  message: {
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text: 'from a sub-agent' }],
  },
});

/**
 * A delivery frame as forge forges it: the reader's words and NO id.
 *
 * Nobody on the server can supply one - the outbound prompt carries none, the
 * CLI mints the transcript's own afterwards, and the CLI never echoes what it
 * was given - so a turn opened by one is matched to its page copy by the
 * frames the two share.
 */
const forged = (text: string): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/**
 * A `user` frame carrying the reader's own words.
 *
 * Its id is derived from the text the way the `turn` helper's is, so a frame
 * and the page row carrying the same words are the same frame.
 */
const typed = (text: string): unknown => ({
  type: 'user',
  uuid: `u-${text}`,
  message: { role: 'user', content: [{ type: 'text', text }] },
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

  it('opens no row for a frame the fold draws nothing for', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // Before any page has landed there is no turn for it to join, and it
    // still opens none: a row it opened would draw nothing.
    server.update({ chat_appended: { key: LEAD, msg: counted(1) } });
    expect(get(chat.value).turns, 'a frame with no turn to join is held nowhere').toEqual([]);

    server.send(page([turn('t1', 'first'), turn('t2', 'second')], null));

    // The thinking-token counter arrives as a `system` frame and the CLI
    // sends one about every fifty tokens, so a running seat receives
    // thousands. Each one used to open a row of its own that the fold
    // renders nothing into - and a row the reader never scrolls to is never
    // measured, so it holds a whole turn's worth of scroll range rather than
    // the 24px it draws at.
    //
    // It is one subtype of sixteen the CLI emits: the rule is the frame's
    // TYPE, and a second subtype here is what keeps it from being read as
    // this counter's name.
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });
    server.update({ chat_appended: { key: LEAD, msg: progressed() } });
    server.update({ chat_appended: { key: LEAD, msg: counted(100) } });

    const after = get(chat.value).turns;
    expect(after.length, 'the frames opened no row of their own').toBe(2);
    expect(after[after.length - 1]?.messages, 'and are held in the turn they arrived in').toEqual([
      ...turn('t2', 'second').messages,
      counted(50),
      progressed(),
      counted(100),
    ]);
  });

  it('opens no row for a frame of any type the fold draws nothing out of', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first'), turn('t2', 'second')], null));

    // A tool result arrives as a `user` frame and a thinking block as an
    // `assistant` one. Neither draws anything, and on a running seat both
    // arrive between one turn and the next, so a row apiece is a blank row per
    // tool call and per thinking block.
    //
    // The last two draw nothing for a reason that is NOT their type - the
    // fold's own monitor guard and its dispatch guard - so a rule keyed on
    // types would open a row for each of them.
    server.update({ chat_appended: { key: LEAD, msg: result('call-1') } });
    server.update({ chat_appended: { key: LEAD, msg: thought() } });
    server.update({ chat_appended: { key: LEAD, msg: monitoring() } });
    server.update({ chat_appended: { key: LEAD, msg: dispatched() } });

    const after = get(chat.value).turns;
    expect(after.length, 'none of the frames opened a row of its own').toBe(2);
    expect(
      after[after.length - 1]?.messages,
      'and all of them are held in the turn they arrived in',
    ).toEqual([
      ...turn('t2', 'second').messages,
      result('call-1'),
      thought(),
      monitoring(),
      dispatched(),
    ]);
  });

  it('opens a row for the reader own words while a turn is live', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // A frame that draws something opens the turn in flight when none is live.
    server.update({ chat_appended: { key: LEAD, msg: said('working') } });
    // Then the reader says something. It starts a turn of its own even though
    // one is being written, because the words belong after the answer rather
    // than inside it.
    server.update({ chat_appended: { key: LEAD, msg: typed('now do this') } });

    const after = get(chat.value).turns;
    expect(after.length, 'the turn in flight and the reader own turn').toBe(3);
    expect(after[1]?.messages, 'the answer holds no part of what was typed').toEqual([
      said('working'),
    ]);
    expect(after[2]?.messages, 'and the words opened a row of their own').toEqual([
      typed('now do this'),
    ]);
  });

  it('does not draw a keyless turn twice when a frame joined it and a page repeats it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // A turn the fold could not name: its key comes from the client.
    server.send(page([turn(null, 'unnamed')], null));
    // A frame joins it, so the turn holds more than the page said it did.
    server.update({ chat_appended: { key: LEAD, msg: result('call-1') } });
    // The next page carries that turn as it now stands.
    server.send(
      page([{ key: null, messages: [...turn(null, 'unnamed').messages, result('call-1')] }], null),
    );

    const after = get(chat.value).turns;
    expect(after.length, 'the repeated turn is the one already held, not a second row').toBe(1);
  });

  it('replaces a turn whose opening frame carries no id', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // A delivery forge forges and sends as a frame: no id at all, because
    // nothing forge holds can mint the one the CLI will give it. The turn it
    // opens is matched to the page's copy by the frames they SHARE, not by the
    // opening one - which is what the two copies agree on either way.
    server.update({ chat_appended: { key: LEAD, msg: forged('typed elsewhere') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    const built = get(chat.value).turns.at(-1)?.key ?? '';

    server.send(
      page([{ key: null, messages: [forged('typed elsewhere'), said('answer-1'), ended()] }], '1'),
    );

    const after = get(chat.value).turns;
    expect(after.map((row) => row.key).includes(built), 'the frame-built row is gone').toBe(false);
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('typed elsewhere')).length,
      'and the turn is held once, as the page has it',
    ).toBe(1);
  });

  it('drops the drawn conversation when the seat changes occupant', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'the previous occupant')], '1'));

    // A `/new`, a `/resume`, a login or a logout: the slot keeps its address
    // and its contents are not the conversation drawn here.
    server.update({ session_replaced: { key: LEAD, session_id: 'new-occupant' } });

    const after = get(chat.value);
    expect(after.turns, 'the previous occupant turns are gone').toEqual([]);
    expect(after.loaded, 'and the column is not claiming to hold a page').toBe(false);
    expect(server.more().length, 'and it asks for the new occupant page').toBe(2);
  });

  it('drops a page an abandoned ask answered after the seat changed occupant', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'the previous occupant')], '1'));

    // The reader reaches the top, which asks for older turns: that ask is in
    // flight when the occupant changes.
    chat.older();
    server.update({ session_replaced: { key: LEAD, session_id: 'new-occupant' } });
    // The page the abandoned ask was waiting for lands after the swap. It
    // carries neither an id nor an occupant, so the only way to know it is not
    // the new occupant's is that an ask was abandoned.
    server.send(page([turn('t0', 'older, previous occupant')], null));

    const after = get(chat.value);
    expect(
      after.turns.map((row) => JSON.stringify(row.messages)),
      'the abandoned answer is not the new occupant conversation',
    ).toEqual([]);
  });

  it('does not let a page read mid-turn split the turn it copies', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // The wire's real order: the CLI never echoes a prompt, so a turn in
    // flight is opened by the ASSISTANT's first frame and the reader's words
    // reach the client only in a page - which is read while the turn is still
    // being written, so it holds the words and the frame they landed before,
    // and nothing after.
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    server.send(
      page([turn('t1', 'first'), { key: null, messages: [typed('mine'), said('answer-1')] }], '1'),
    );

    // The rest of the turn arrives after that page.
    server.update({ chat_appended: { key: LEAD, msg: said('answer-2') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-3') } });

    const rows = get(chat.value).turns.map((row) => JSON.stringify(row.messages));
    const mine = rows.filter((messages) => messages.includes('mine'));
    expect(mine.length, 'the reader words and the whole answer are one row').toBe(1);
    expect(mine[0], 'and that row carries the frames that followed the page').toContain('answer-3');
    expect(
      rows.filter((messages) => messages.includes('answer-2')).length,
      'with no second row holding the tail',
    ).toBe(1);
  });

  it('keeps the turn being written when a page of older turns lands', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], '1'));

    // The wire's real order: the CLI never echoes a prompt, so the turn in
    // flight is opened by the assistant's first frame.
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    // The reader scrolls up, which asks for the turns above what is held. That
    // page is the fold's account of OLDER turns, so it cannot carry this one -
    // and dropping it there leaves the answer nowhere, with nothing asking the
    // server for it again.
    chat.older();
    server.send(page([turn('t0', 'older')], null));

    const held = get(chat.value).turns.map((row) => JSON.stringify(row.messages));
    expect(
      held.some((messages) => messages.includes('answer-1')),
      'an older page does not drop the turn being written',
    ).toBe(true);
    expect(get(chat.value).turns[0]?.key, 'and the older page still landed above').toBe('t0');
  });

  it('keeps a live turn a newest page was serialized without', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    // A newest page that repeats what it held before that frame landed.
    server.send(page([turn('t1', 'first')], '1'));

    const held = get(chat.value).turns.map((row) => JSON.stringify(row.messages));
    expect(
      held.some((messages) => messages.includes('answer-1')),
      'a page that does not share a frame with the turn does not drop it',
    ).toBe(true);
  });

  it('replaces a live turn with the page that settled it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    const built = get(chat.value).turns.at(-1)?.key ?? '';

    // The fold's own account of that turn, and settled: it carries the result
    // frame, so it is not a page read while the turn was still being written.
    server.send(
      page(
        [turn('t1', 'first'), { key: null, messages: [typed('mine'), said('answer-1'), ended()] }],
        '1',
      ),
    );

    const after = get(chat.value).turns;
    expect(after.map((row) => row.key).includes(built), 'the frame-built row is gone').toBe(false);
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('answer-1')).length,
      'and the turn is held once, as the page has it',
    ).toBe(1);
  });

  it('replaces a live turn from the page own copy of a row it already holds', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));
    // A page read before the answer: the row this client holds carries the
    // reader's words and nothing else, which is what a read taken early gives.
    server.send(page([{ key: null, messages: [typed('mine')] }], '1'));
    const held = get(chat.value).turns.at(-1)?.key ?? '';
    // The answer then arrives as frames, opening a live turn over that row.
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-2') } });

    // The next page repeats that row, and its own copy carries the frames. A
    // repeated row is handed back as the object already held, whose messages
    // are older - asking THAT object is asking the wrong copy.
    server.send(
      page(
        [{ key: null, messages: [typed('mine'), said('answer-1'), said('answer-2'), ended()] }],
        '1',
      ),
    );

    const rows = get(chat.value).turns.map((row) => JSON.stringify(row.messages));
    expect(
      rows.filter((messages) => messages.includes('answer-2')).length,
      'the turn is held once, not as a stale copy beside a live one',
    ).toBe(1);
    expect(rows.length, 'and no extra row survives it').toBe(2);
    expect(held, 'the row kept the name the page gave it').toBeDefined();
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

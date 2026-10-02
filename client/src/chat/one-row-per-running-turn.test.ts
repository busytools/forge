import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import type { ServerMessage, SessionUpdate } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat, beingWritten, type PageTurn, type Turn } from './conversation';
import { fold, type Unit } from './units';

/**
 * One row per running turn, carried from open to settle.
 *
 * The row has to draw the bar (the ring, the elapsed clock, `thinking N`),
 * and nothing arriving mid-turn may split it in two. A turn the client watched
 * open is the frames path; a turn it reached mid-flight has its row from a
 * page, and the seat's own answer - `header.turn_in_flight` - is what says so.
 * The answer is re-read rather than stamped, so a page that lands before it
 * still gets the bar, and a turn that ends takes it back.
 */

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
const OTHER: SessionSlot = { org: 'Busytools', project: 'forge', label: 'other' };

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

/** A prompt as forge echoes it for a view: the words and NO id. */
const forged = (text: string): unknown => ({
  type: 'user',
  timestamp: '2026-10-01T10:00:02Z',
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

/** The frame a turn ends on. */
const ended = (): unknown => ({
  type: 'result',
  uuid: 'r-1',
  subtype: 'success',
  is_error: false,
});

/**
 * A mid-turn prompt as a PAGE holds it: the scan hoists the transcript's
 * `attachment` row into a user envelope carrying this block, and the fold opens
 * a turn on it.
 *
 * `id` is the row's own, kept apart from the words so a test counting one does
 * not count the other.
 */
const queued = (text: string, id: string): unknown => ({
  type: 'user',
  uuid: `q-${id}`,
  timestamp: '2026-10-01T10:00:02Z',
  message: {
    role: 'user',
    content: [{ type: 'queued_command', prompt: text, commandMode: 'prompt' }],
  },
});

/** The frame the CLI gives up with, after which no result follows. */
const failed = (): unknown => ({ type: 'error', error: 'read loop died' });

/** The frame the CLI re-fires at the head of every turn. */
const began = (): unknown => ({
  type: 'system',
  subtype: 'init',
  session_id: 's',
  uuid: 'init-1',
});

/**
 * A connection a test drives by hand, as `conversation.test.ts` has one: what
 * the chat needs is what it asked for, the messages it was sent, and the seat
 * whose subject each one belongs to.
 *
 * `held` is the seat's own store as a visited seat leaves it - its last answer
 * and the frames since - and `null` is a seat nothing has answered yet.
 */
function fakeConnection(held: Holding | null = null) {
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
    store: () => (held === null ? undefined : seatStore(held)),
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
    /** A seat's own record, which is where the core's answer for a turn crosses. */
    says(running: boolean, seat: SessionSlot = LEAD): void {
      this.send({
        kind: 'snapshot',
        subject: { session: seat },
        data: { header: { turn_in_flight: running } },
      });
    },
  };
}

/** A seat's own store as a visited seat leaves it: its last answer, and the frames since. */
interface Holding {
  running: boolean;
  /** The frames the store has taken since that answer, oldest first. */
  updates?: SessionUpdate[];
  /** How many of the oldest frames its cap has already dropped. */
  dropped?: number;
}

const seatStore = (held: Holding) => ({
  snapshot: () => ({ header: { turn_in_flight: held.running } }),
  updates: () => held.updates ?? [],
  dropped: () => held.dropped ?? 0,
});

const page = (turns: PageTurn[], cursor: string | null): ServerMessage => ({
  kind: 'page',
  conversation: LEAD,
  turns,
  cursor,
});

/** Whether the draw carries the bar, asked the way the row asks it. */
const bar = (turn: Turn | undefined): boolean =>
  turn !== undefined && running(fold(turn.messages, null, null, beingWritten(turn)));

/** Whether a row is the turn being written, asked the way the column asks it. */
const writing = (turn: Turn | undefined): boolean => turn?.running === true;

/** Whether the units carry the bar: a running report row. */
const running = (units: Unit[]): boolean =>
  units.some((unit) => unit.kind === 'report' && unit.info.running);

/** The turn a chat is holding, newest last. */
const newest = (chat: Chat): Turn | undefined => get(chat.value).turns.at(-1);

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

    expect(newest(chat)?.live, 'the turn the frames opened is live').toBe(true);
    expect(bar(newest(chat)), 'the bar').toBe(true);
  });

  it('draws the bar for a running turn the client learned from a page', () => {
    // What a page open mid-turn really sees: the newest page row IS the turn
    // being written, and a page read while a turn runs carries no result frame
    // for it. The counters then arrive - `system` frames that draw nothing on
    // their own.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);

    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });

    expect(writing(newest(chat)), 'the row the page carried is the running turn').toBe(true);
    expect(bar(newest(chat)), 'the bar').toBe(true);
  });

  it('reads the seat answer after the page, not at it', () => {
    // The order a fresh open really has: `more` is asked before the seat is
    // subscribed, so the page lands with nothing to read yet - and the
    // subscribe's own answer arrives a round trip behind it.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    expect(writing(newest(chat)), 'nothing has answered yet').toBe(false);

    server.says(true);

    expect(writing(newest(chat)), 'the answer brings the bar').toBe(true);
    expect(bar(newest(chat)), 'and it draws').toBe(true);
  });

  it('holds a running turn the page carried in one row, with its bar', () => {
    // When the thought lands, the drawing frame joins the row the page carried
    // rather than opening a second one.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });
    server.update({ chat_appended: { key: LEAD, msg: said('the thought lands') } });
    server.update({ chat_appended: { key: LEAD, msg: counted(50) } });

    const turns = get(chat.value).turns;
    expect(turns, 'one row, the one the page carried').toHaveLength(1);
    // And it is THAT row: without this, a row the frames opened on their own
    // would satisfy the count, the liveness and the bar alike.
    expect(turns[0]?.key, "the page's own row").toBe('t1');
    expect(writing(turns[0]), 'and it is the running turn').toBe(true);
    expect(bar(turns[0]), 'with the bar').toBe(true);
  });

  it('leaves finished history alone when an older page lands mid-turn', () => {
    // The reader scrolls up on a seat that is running: the older page's own
    // last row is simply the turn above the window, and a transcript-derived
    // page carries no result frame at all - so nothing in it is the turn being
    // written.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], '9'));
    server.says(true);

    chat.older();
    server.send(
      page(
        [
          { key: 't0', messages: [typed('older')] },
          { key: 't0b', messages: [said('older answer')] },
        ],
        '5',
      ),
    );

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'the older rows above, the running turn last',
    ).toEqual(['t0', 't0b', 't1']);
    expect(
      turns.filter((turn) => turn.running === true).map((turn) => turn.key),
      'and only the newest row is the turn being written',
    ).toEqual(['t1']);
  });

  it('carries a prompt arriving mid-turn into the running row', () => {
    // What the reader does mid-turn: they type while the turn is still
    // running. The CLI fuses that prompt into the turn it interrupted rather
    // than opening one, and the terminal draws it that way - the running row
    // keeps its clock and the words draw inside it. A row of its own here puts
    // two rows of one turn on the page, both counting.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);

    server.update({ chat_appended: { key: LEAD, msg: typed('now do this') } });

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'the running turn took the words',
    ).toEqual(['t1']);
    expect(turns[0]?.messages, 'and they drew inside it').toEqual([
      typed('mine'),
      said('working'),
      typed('now do this'),
    ]);
    expect(writing(turns[0]), 'the row the turn opened in is still the one being written').toBe(
      true,
    );
    expect(bar(turns[0]), 'with the bar it never gave up').toBe(true);
  });

  it('opens a row for a prompt once the running row carries its own end', () => {
    // The other side of the same rule, and the reason it is not just "the row
    // is live": a turn the frames built stays live for good, so a prompt
    // arriving after its result landed must open the next turn's row rather
    // than be swallowed by the one that is over.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('first')] }], null));

    server.update({ chat_appended: { key: LEAD, msg: typed('mine') } });
    server.update({ chat_appended: { key: LEAD, msg: said('working') } });
    server.update({ chat_appended: { key: LEAD, msg: ended() } });
    server.update({ chat_appended: { key: LEAD, msg: typed('now do this') } });

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'the words opened the turn after the one that ended',
    ).toEqual(['t1', 'live:u-mine', 'live:u-now do this']);
    expect(turns[1]?.messages, 'and the turn that ended kept only its own frames').toEqual([
      typed('mine'),
      said('working'),
      ended(),
    ]);
    expect(turns[2]?.messages, 'the new row holds the words and nothing before them').toEqual([
      typed('now do this'),
    ]);
  });

  it('keeps one row when a page lands with the fold own cut at the words', () => {
    // A page is read from the transcript, where the CLI persists a mid-turn
    // prompt as an `attachment` row - and the fold opens a turn on the block
    // the scan hoists out of it, so the page carries the words as a row with
    // its own answer under them. The turn it interrupted is the turn it
    // belongs to: the live path draws it inside that row, and the page's cut
    // is joined back the same way rather than re-splitting the row.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], '9'));
    server.says(true);
    server.update({ chat_appended: { key: LEAD, msg: forged('now do this') } });
    server.update({ chat_appended: { key: LEAD, msg: said('more') } });
    server.update({ chat_appended: { key: LEAD, msg: ended() } });
    expect(newest(chat)?.key, 'precondition: the words joined the running row').toBe('t1');

    // The row the fold cut is unnamed, as every turn of a transcript read is:
    // its name comes from its own first frame, which is how the conversation
    // holds it when the page repeats it.
    const cut = [
      { key: 't1', messages: [typed('mine'), said('working')] },
      { key: null, messages: [queued('now do this', '1'), said('more'), ended()] },
    ];
    server.send(page(cut, null));

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'one row, and the page own cut added no name to it',
    ).toEqual(['t1']);
    expect(
      JSON.stringify(turns[0]?.messages).split('now do this').length - 1,
      'the words drawn once, not once per carrier',
    ).toBe(1);
    expect(turns[0]?.messages, 'the row is the one the live path wrote').toEqual([
      typed('mine'),
      said('working'),
      forged('now do this'),
      said('more'),
      ended(),
    ]);

    // The next page repeats the row, as the server's cut does, and a row the
    // conversation does not recognize by the page's name for it is drawn a
    // second time.
    server.send(page(cut, null));

    expect(
      get(chat.value).turns.map((turn) => turn.key),
      'the repeated cut is the row already held, not a second one',
    ).toEqual(['t1']);
  });

  it('draws a second prompt saying the same words', () => {
    // Two prompts saying "yes" inside one turn are two messages, and the page
    // carries them as TWO ROWS: the fold opens a turn on every queued block,
    // so no page can hold two of them in one row. The pairing is counted across
    // the whole page rather than per row, so neither copy consumes the frame
    // the other is the same message as - the reader's own words, gone.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine')] }], null));
    server.says(true);
    // The row grew with the second prompt's own frame, and nothing pairs with
    // a frame twice: the first page row's copy is the one that has it.
    server.update({ chat_appended: { key: LEAD, msg: forged('yes') } });

    server.send(
      page(
        [
          { key: 't1', messages: [typed('mine')] },
          { key: null, messages: [queued('yes', '1'), said('more')] },
          { key: null, messages: [queued('yes', '2'), said('even more')] },
        ],
        null,
      ),
    );

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'both prompts landed in the turn they interrupted',
    ).toEqual(['t1']);
    expect(
      JSON.stringify(turns[0]?.messages).split('"yes"').length - 1,
      'and the words are drawn twice, once per prompt',
    ).toBe(2);
  });

  it('keeps both prompts when a page holds no frame to pair them with', () => {
    // The same two rows on a seat whose prompt frames this client never saw,
    // which a page read from the transcript is: nothing pairs with anything,
    // and the copy added for the first row must not answer for the second.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine')] }], null));
    server.says(true);

    server.send(
      page(
        [
          { key: 't1', messages: [typed('mine')] },
          { key: null, messages: [queued('yes', '1'), said('more')] },
          { key: null, messages: [queued('yes', '2'), said('even more')] },
        ],
        null,
      ),
    );

    const turns = get(chat.value).turns;
    expect(
      turns.map((turn) => turn.key),
      'both prompts landed in the turn they interrupted',
    ).toEqual(['t1']);
    expect(
      JSON.stringify(turns[0]?.messages).split('"yes"').length - 1,
      'and both are drawn, neither read as the other',
    ).toBe(2);
  });

  it('takes the bar back when the turn ends', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);
    expect(writing(newest(chat)), 'precondition: the bar is up').toBe(true);

    server.update({ chat_appended: { key: LEAD, msg: ended() } });

    expect(writing(newest(chat)), 'the result ended the turn').toBe(false);
    expect(bar(newest(chat)), 'so the bar is gone').toBe(false);
  });

  it('reads a seat returned to after its turn died on the error frame', () => {
    // The store's answer is its last snapshot stepped by the frames since, and
    // the CLI's error - after which no result follows - is a frame that ends
    // the turn. Read here rather than through a live frame because this is the
    // path where the rule is alone: the page carries no frame that ended the
    // turn, so nothing else can say the bar is over.
    const server = fakeConnection({
      running: true,
      updates: [{ chat_appended: { key: LEAD, msg: failed() } }],
    });
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    expect(newest(chat)?.key, "the page's row is there").toBe('t1');
    expect(writing(newest(chat)), 'the error frame ended the turn').toBe(false);
  });

  it('reads a seat already visited from what its own store holds', () => {
    // A return subscribes nothing - the subscription is the client's for the
    // life of the connection - so the store is the only thing that can say, and
    // it has the last answer stepped by every frame since.
    const server = fakeConnection({ running: true });
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    expect(writing(newest(chat)), 'the bar is up from the seat the client kept').toBe(true);
  });

  it('holds one row when the newest page repeats the running turn', () => {
    // What a reconnect does: `start`'s status handler asks the newest page
    // again, and the newest page is built from the live conversation, so it
    // repeats the turn the client already holds under the same key.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], '9'));
    server.says(true);

    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], '9'));

    expect(
      get(chat.value).turns.map((turn) => turn.key),
      'one row under one key',
    ).toEqual(['t1']);
  });

  it('draws the bar for a turn the frames opened with their own init', () => {
    // The other way a turn opens, and the only one when no seat answer is in
    // reach: the `init` the CLI re-fires at the head of every turn says a turn
    // has begun, and the row the page carried is where it draws.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    expect(writing(newest(chat)), 'precondition: nothing is running yet').toBe(false);

    server.update({ chat_appended: { key: LEAD, msg: began() } });

    expect(writing(newest(chat)), 'the init opened a turn').toBe(true);
    expect(bar(newest(chat)), 'and the bar draws on the row it landed in').toBe(true);
  });

  it('reads a seat returned to after its turn ended while the page was away', () => {
    // The store's answer is its last snapshot stepped by the frames since, and
    // those frames can end the turn the snapshot had running.
    const server = fakeConnection({
      running: true,
      updates: [{ chat_appended: { key: LEAD, msg: ended() } }],
    });
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    // The floor, and it has to be this one: no turn is running in this case by
    // construction, so an assertion that the bar is up has nothing to say. What
    // a missing row would otherwise pass as is a settled one.
    expect(newest(chat)?.key, "the page's row is there").toBe('t1');
    expect(writing(newest(chat)), 'the turn ended while the page was away').toBe(false);
  });

  it('answers from the snapshot when the store dropped its oldest frames', () => {
    // The store's cap takes the OLDEST off the front, and the `init` that opens
    // a turn is the front of it - so a tail that lost frames is not replayed at
    // all, and the snapshot answers instead.
    const server = fakeConnection({
      running: true,
      updates: [{ chat_appended: { key: LEAD, msg: ended() } }],
      dropped: 3,
    });
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    expect(writing(newest(chat)), 'the snapshot answers, incomplete tail and all').toBe(true);
  });

  it('takes the bar back when the core says the turn errored', () => {
    // A take-back with no result frame behind it: the update the server sends
    // when a turn ends badly is the only thing that says so.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);
    expect(writing(newest(chat)), 'precondition: the bar is up').toBe(true);

    server.update({ turn_error: { key: LEAD } });

    expect(writing(newest(chat)), 'the turn is over').toBe(false);
  });

  it('does not raise the bar again on a row that already ended', () => {
    // A snapshot taken while the turn ran can land after the result did. The
    // row carries the frame it ended on, and nothing puts the bar back.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);
    expect(writing(newest(chat)), 'precondition: the bar is up').toBe(true);

    server.update({ chat_appended: { key: LEAD, msg: ended() } });
    expect(writing(newest(chat)), 'the result ends the turn').toBe(false);

    server.says(true);

    expect(writing(newest(chat)), 'a row carrying its result is over for good').toBe(false);
  });

  it('leaves the bar off a row that carries the error it died on', () => {
    // A page can carry the turn that died on the CLI's error while the seat's
    // answer says the next turn is running - and a row whose own frames carry
    // its end is over whatever the seat says.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working'), failed()] }], null));
    server.says(true);

    expect(newest(chat)?.key, "the page's row is there").toBe('t1');
    expect(writing(newest(chat)), 'a row carrying its error is over').toBe(false);
    expect(bar(newest(chat)), 'and no bar draws on it').toBe(false);
  });

  it('does not carry the bar onto the occupant that replaced it', () => {
    // A `/new` under a seat that was running: what answered for the old
    // occupant says nothing about the one that took the seat.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));
    server.says(true);
    expect(writing(newest(chat)), 'precondition: the bar is up').toBe(true);

    server.update({ session_replaced: { key: LEAD, session_id: 'new' } });
    server.send(page([{ key: 'n1', messages: [typed('mine'), said('an old answer')] }], null));

    expect(writing(newest(chat)), 'nothing about the new occupant is known yet').toBe(false);
  });

  it('ignores another seat answer', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([{ key: 't1', messages: [typed('mine'), said('working')] }], null));

    server.says(true, OTHER);

    // The floor, for the same reason its sibling carries one: an absent row
    // would answer the assertion below as a settled turn does.
    expect(newest(chat)?.key, "the page's row is there").toBe('t1');
    expect(writing(newest(chat)), 'the answer for this seat, and no other').toBe(false);
  });
});

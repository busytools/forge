// @vitest-environment jsdom
// jsdom for the painted frame the fold's draws land on (requestAnimationFrame),
// which a stream frame's publish and its tests both wait for (#1670).
import { get } from 'svelte/store';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { MORE_TURNS, subjectKey } from '../protocol';
import type { ClientMessage, ServerMessage, SessionUpdate } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { Chat, type PageTurn } from './conversation';
import { echoes } from './echoes.svelte';
import { outputs } from './outputs.svelte';
import { refused } from '../refusals';

// Every record the class publishes is frozen, so an in-place edit where a
// record should have been replaced throws here as well as in a mounted column.
vi.mock('./conversation', async (importOriginal) => {
  const { frozenConversation } = await import('./testing/frozen');
  return frozenConversation(await importOriginal<typeof import('./conversation')>());
});
import { fold } from './units';

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

// A test that fails part-way through a fake-timer case would otherwise leave
// every later case in this file on fake time.
afterEach(() => {
  vi.useRealTimers();
});

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

/** An `assistant` frame whose only call is a monitor, the call the fold draws. */
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

/** A compaction boundary, which the CLI persists to the transcript too. */
const boundary = (): unknown => ({
  type: 'system',
  subtype: 'compact_boundary',
  uuid: 'compact-1',
  compact_metadata: { trigger: 'auto', pre_tokens: 10, post_tokens: 2 },
});

/** One hook run, which no page copy of a turn carries. */
const hookRun = (): unknown => ({
  type: 'system',
  subtype: 'hook_started',
  hook_id: 'hook-1',
  hook_name: 'PreToolUse',
  uuid: 'hook-1-uuid',
});

/**
 * A forged row with NO id: the arm a frame without one still reconciles
 * through.
 *
 * The live forge now stamps every forged prompt row with the prompt's uuid,
 * so a turn opened by one reconciles by id - but the prose arm is what a
 * frame carrying neither id nor match must still fall back to, and this
 * helper is that frame.
 */
const forged = (text: string): unknown => ({
  type: 'user',
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/** The same words as the CLI persisted them, carrying the id the CLI minted. */
const minted = (text: string): unknown => ({
  type: 'user',
  uuid: `c-${text}`,
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
  const statuses = new Set<(status: ConnectionStatus) => void>();
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
    /** The connection's own life, which the column watches for a reconnect. */
    reach(status: ConnectionStatus): void {
      for (const fn of statuses) fn(status);
    },
    /** A refusal, which is the answer a page is not. */
    refuse(what: string, why: string): void {
      this.send({ kind: 'error', what, why });
    },
  };
}

/** The core's own turn for words nobody typed, carrying the prompt's id. */
const forgedUnder = (text: string, id: string): unknown => ({
  type: 'user',
  uuid: id,
  message: { role: 'user', content: [{ type: 'text', text }] },
});

const words = (chat: Chat): string => JSON.stringify(get(chat.value).turns);

describe('the conversation the chat draws', () => {
  /**
   * A seat the reader has left still FOLDS.
   *
   * **The state is lossless for every seat; whether one is shown gates only
   * the draw** (Ved, 2026-10-09). The frames arrive for every visited seat
   * through the home feed anyway - dropping them here made the state the
   * place a conversation was lost, because the return's reads cannot restore
   * what the bounded window has itself already dropped. The column keeps a
   * left seat's value without drawing it.
   */
  it('folds while unshown - the draw is what waits, never the state', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'kept')], null));

    server.update({ chat_appended: { key: LEAD, msg: said('while shown') } });

    chat.leaving();
    server.update({ chat_appended: { key: LEAD, msg: said('while away') } });
    expect(
      JSON.stringify(get(chat.value).turns),
      'a frame for an unshown seat folds into the state',
    ).toContain('while away');

    chat.showing();
    expect(words(chat), 'and the return draws it').toContain('while away');
    stop();
  });

  /**
   * A swap that lands while the seat is away is structural, so it folds
   * anyway: the held conversation empties and the return's page - the NEW
   * occupant's - merges onto nothing. Gated, the swap never ran and the
   * return merged the new occupant's page onto the old one's turns, which
   * the keep-above arm then kept: both drew, persistently, and no later
   * read heals it because the swap frame is never re-sent.
   */
  it('empties the conversation on a swap that lands while the seat is away', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'the old occupant')], null));

    chat.leaving();
    const asked = server.more().length;
    server.update({ session_replaced: { key: LEAD } });
    expect(server.more().length, 'the swap runs while away and asks for the new occupant').toBe(
      asked + 1,
    );
    chat.showing();
    server.send(page([turn('t2', 'the new occupant')], null));

    const drawn = words(chat);
    expect(drawn, 'the old occupant is gone').not.toContain('the old occupant');
    expect(drawn, 'and the new one draws').toContain('the new occupant');
    stop();
  });

  /**
   * **A page the client's half reconciled is the catch-up, not an answer.**
   * After a park, an ask that died with the connection leaves the count of
   * abandoned asks standing - and the reconcile's newest window would be
   * swallowed by it, the reader meeting an old conversation with no way to
   * tell. The mark is what tells the two apart; without it this page
   * disappears, which is the case beside this one.
   */
  it('lands a reconciled page even when an ask died with the park', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    // A page with a cursor, so an older ask can go out and never answer.
    server.send(page([turn('t1', 'the kept turn')], 'cursor-1'));
    chat.older();
    server.update({ session_replaced: { key: LEAD } });

    server.send({
      kind: 'page',
      conversation: LEAD,
      turns: [turn('t9', 'the catch-up')],
      cursor: null,
      reconciled: true,
    });

    const drawn = words(chat);
    expect(drawn, 'the catch-up lands as the newest window').toContain('the catch-up');
    stop();
  });

  /** The same state, unmarked: the abandoned count swallows it (the pair
   *  with the test above is what makes the mark load-bearing). */
  it('swallows an unmarked page while an ask is abandoned', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'the kept turn')], 'cursor-1'));
    chat.older();
    server.update({ session_replaced: { key: LEAD } });

    server.send(page([turn('t9', 'not an answer')], null));

    expect(words(chat), 'an answer to an ask nobody wants is dropped').not.toContain(
      'not an answer',
    );
    stop();
  });

  /**
   * A return whose page does not reach the held tail KEEPS the tail and
   * walks back to it. Dropping it (the old contract) made the stretch
   * between the page and the tail unreachable except by a scroll, and the
   * tail itself was held state no read owed anyone (Ved, 2026-10-09). The
   * page's own cursor is the walk, so the pages between load on their own.
   */
  it('keeps the unreached tail and walks back to it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    // Two turns, so the fresh open's fill has nothing to ask for - the return's
    // own newest page is the landing that has to reach back.
    server.send(page([turn('t1', 'kept tail'), turn('t1b', 'kept too')], 'tail-cursor'));

    chat.leaving();
    chat.showing();
    server.send(page([turn('t2', 'a new turn')], 'newest-cursor'));

    expect(words(chat), 'the unreached tail is kept').toContain('kept tail');
    expect(words(chat), 'and the newest page draws').toContain('a new turn');
    expect(
      server.more().at(-1),
      'and the walk asks the next page down from the one that could not reach',
    ).toEqual({ kind: 'more', conversation: LEAD, before: 'newest-cursor', turns: 20 });
    stop();
  });

  /**
   * The occlusion repro: a SHOWN seat whose socket dies (the app's window
   * was behind something and its page was suspended) and reconnects. The
   * frames that crossed while the socket was down are the ones no live fold
   * ever sees - the reconnect's newest page is the only carrier - so every
   * one of them must land in the merge, whatever the page's cut did to the
   * live turn's row (#1893's machinery) and whatever the held row already
   * carried.
   */
  it('a shown seat that reconnects keeps every frame that crossed while the socket was down', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'before the drop')], 'c1'));
    server.update({ chat_appended: { key: LEAD, msg: said('held before the drop') } });

    // The window was occluded: the socket died and came back.
    server.reach('connecting');
    server.reach('open');
    expect(server.more().at(-1), 'the reconnect asks the newest page').toEqual({
      kind: 'more',
      conversation: LEAD,
      before: null,
      turns: 20,
    });

    // The page as the transcript has it - the held frames and the ones that
    // crossed while the socket was down, one turn.
    server.send(
      page(
        [
          {
            key: 't1',
            messages: [
              {
                type: 'user',
                uuid: 'u-before',
                message: { role: 'user', content: [{ type: 'text', text: 'before the drop' }] },
              },
              {
                type: 'assistant',
                uuid: 'a-held',
                message: {
                  role: 'assistant',
                  content: [{ type: 'text', text: 'held before the drop' }],
                },
              },
              {
                type: 'assistant',
                uuid: 'a-gap',
                message: {
                  role: 'assistant',
                  content: [{ type: 'text', text: 'crossed while down' }],
                },
              },
            ],
          },
        ],
        null,
      ),
    );

    const drawn = words(chat);
    expect(drawn, 'the frame that crossed while down must draw').toContain('crossed while down');
    expect(drawn, 'and the held frame stays').toContain('held before the drop');
    stop();
  });

  /**
   * The reconnect whose newest page stops at the window's floor: the page
   * does not reach the held state, so the stretch in between - the frames
   * that crossed while the socket was down - is in no page the client has.
   * The held tail must not be dropped and the read must not stop at the
   * floor: the conversation walks back until a page reaches what it holds,
   * and every frame lands.
   */
  it('walks below the floor when the newest page does not reach the held state', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'oldest words')], 'c1'));
    server.update({ chat_appended: { key: LEAD, msg: said('held before the drop') } });

    // The socket died while the page was suspended; frames crossed unheld.
    server.reach('connecting');
    server.reach('open');

    // The newest page, cut at the window's floor: it carries only the newest
    // stretch - the held turn is below it, and the gap is between them.
    server.send(
      page(
        [
          {
            key: 't2',
            messages: [
              {
                type: 'assistant',
                uuid: 'a-newest',
                message: { role: 'assistant', content: [{ type: 'text', text: 'newest stretch' }] },
              },
            ],
          },
        ],
        'floor-cursor',
      ),
    );

    // The walk: the conversation asks the next page down, on its own.
    const walked = server.more().filter((ask) => ask.kind === 'more' && ask.before !== null);
    expect(
      walked.length,
      'a page that does not reach the held state walks back on its own',
    ).toBeGreaterThan(0);
    expect(walked.at(-1)?.before, 'from the page it could not reach from').toBe('floor-cursor');

    // The walked page: the middle - held and gap - as the transcript has it.
    server.send(
      page(
        [
          {
            key: 't1',
            messages: [
              {
                type: 'user',
                uuid: 'u-oldest',
                message: { role: 'user', content: [{ type: 'text', text: 'oldest words' }] },
              },
              {
                type: 'assistant',
                uuid: 'a-held',
                message: {
                  role: 'assistant',
                  content: [{ type: 'text', text: 'held before the drop' }],
                },
              },
              {
                type: 'assistant',
                uuid: 'a-gap',
                message: {
                  role: 'assistant',
                  content: [{ type: 'text', text: 'crossed while down' }],
                },
              },
            ],
          },
        ],
        'c1',
      ),
    );

    const drawn = words(chat);
    expect(drawn, 'the oldest held row stays').toContain('oldest words');
    expect(drawn, 'the frame from the gap draws').toContain('crossed while down');
    expect(drawn, 'and the newest page stays').toContain('newest stretch');
    stop();
  });

  /**
   * The walk fills the gap IN PLACE. A walk page is the middle of the
   * conversation - newer than the tail it walks toward, older than the page
   * it walks from - so the plain older landing (which prepends above
   * everything held) drew the recovered stretch at the top of the column.
   */
  it('draws the walked gap between the tail and the newest page', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'one'), turn('t2', 'two')], 'c-tail'));

    server.reach('connecting');
    server.reach('open');
    // The newest page, cut at the window's floor: the walk starts toward t2.
    server.send(page([turn('t5', 'five')], 'floor'));
    // The walk's pages, from the page's cursor down - the last one reaches
    // the target and stops the walk.
    server.send(page([turn('t4', 'four')], 'floor-2'));
    const beforeReach = server.more().length;
    server.send(page([turn('t2', 'two'), turn('t3', 'three')], 'c-tail'));

    const keys = get(chat.value).turns.map((row) => row.key);
    expect(keys, 'the recovered stretch draws where it happened').toEqual([
      't1',
      't2',
      't3',
      't4',
      't5',
    ]);
    expect(server.more().length, 'and a page reaching the target stops the walk').toBe(beforeReach);
    stop();
  });

  /**
   * The walk stops where the history ends: a page with no cursor above it
   * cannot be walked past, and asking again would fetch the same answer.
   */
  it('stops the walk where the history ends', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'one')], 'c-tail'));

    server.reach('connecting');
    server.reach('open');
    server.send(page([turn('t4', 'four')], 'floor'));
    const beforeEnd = server.more().length;
    server.send(page([turn('t2', 'two'), turn('t3', 'three')], null));

    const keys = get(chat.value).turns.map((row) => row.key);
    expect(keys, 'everything walked still draws').toEqual(['t1', 't2', 't3', 't4']);
    expect(server.more().length, 'and no further ask follows the end').toBe(beforeEnd);
    stop();
  });

  /**
   * A return page heals a held live turn whose counter frames it missed.
   *
   * The strip draws its clock from the held row and its thinking count from
   * the counter frames the row carries - so a held turn that grew without
   * them (the frames crossed while the seat was away) shows a ticking clock
   * and no count until a page puts them back. Ved saw exactly that on
   * 2026-10-09: "the duration just appeared... thinking tokens does not
   * appear."
   */
  it('heals a held live turn with the counters a return page carries', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'one')], 'c1'));
    server.update({ chat_appended: { key: LEAD, msg: said('working') } });

    chat.leaving();
    chat.showing();
    server.send(
      page([{ key: 't1', messages: [typed('one'), said('working'), counted(40)] }], 'c1'),
    );

    const turns = get(chat.value).turns;
    const units = fold(turns[turns.length - 1]?.messages ?? [], null, true, false);
    const report = units.at(-1) as
      { kind?: string; info?: { thinking_tokens?: number | null } } | undefined;
    expect(
      report?.info?.thinking_tokens,
      'the counters the page carries reach the running row',
    ).toBe(40);
    stop();
  });

  /**
   * A completion keeps the live row while the record still says a turn is in
   * flight: the CLI ends a turn per delivered prompt, so a mid-turn message
   * fires `turn_complete` while the seat is still running - and taking that
   * frame as the end killed the clock and the thinking count until a
   * remount (Ved, 2026-10-09: "the thinking tokens... is also very finicky").
   */
  it('keeps the live row through a completion the record says is not the end', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'one')], null));
    server.update({ chat_appended: { key: LEAD, msg: said('working') } });
    // The core's own record says the seat has a turn in flight.
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: true } },
    });

    server.update({ turn_complete: { key: LEAD } });

    expect(
      get(chat.value).turns.at(-1)?.running,
      'the completion is one queued prompt ending, not the seat',
    ).toBe(true);
    stop();
  });

  /**
   * A fresh open keeps asking until history joins the newest turn.
   *
   * A page is twenty TURNS - and a seat whose running turn is giant fills
   * that whole page alone, so a first open drew the newest turn and nothing
   * above it (Ved, 2026-10-09: "it is only showing me the latest one... not
   * able to pull down the older messages"). The fill asks for what is above
   * once, from the page's own cursor, and stops as soon as a landing brings
   * settled turns with it.
   */
  it('a fresh open keeps asking until history joins the newest turn', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t9', 'the newest turn')], 'above-cursor'));
    expect(
      server.more().at(-1),
      'a page that brought nothing but the newest turn asks for the history above it',
    ).toEqual({ kind: 'more', conversation: LEAD, before: 'above-cursor', turns: 20 });

    const asked = server.more().length;
    server.send(page([turn('t7', 'seven'), turn('t8', 'eight')], 'older-cursor'));
    const keys = get(chat.value).turns.map((row) => row.key);
    expect(keys, 'the history above draws with the newest turn').toEqual(['t7', 't8', 't9']);
    expect(server.more().length, 'and the fill stops once a landing brings history with it').toBe(
      asked,
    );
    stop();
  });

  /**
   * A tail no page can carry walks nothing: a `forge_notice` row is written
   * on this page and nowhere else, so targeting one would fetch the whole
   * history one page per landing for a turn no transcript holds.
   */
  it('walks nothing for a tail that is only notices', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.update({
      chat_appended: {
        key: LEAD,
        msg: { type: 'system', subtype: 'forge_notice', severity: 'warning', text: 'a notice' },
      },
    });

    const asked = server.more().length;
    // Two turns on the first page, so the fresh fill cannot fire and the only
    // ask this test can see is a walk's.
    server.send(page([turn('t2', 'a new turn'), turn('t2b', 'and another')], 'floor'));
    expect(
      server.more().length,
      'a notice-only tail has nothing in the transcript to walk for',
    ).toBe(asked);
    expect(words(chat), 'and the notice still draws').toContain('a notice');
    stop();
  });

  /**
   * A return whose ask was swallowed by one in flight fires when that ask
   * settles: the stale page is not the read a return needs, and the want
   * has to outlive it.
   */
  it("fires the return's ask once the in-flight one settles", () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'first')], null));

    chat.refresh();
    const asked = server.more().length;

    chat.leaving();
    chat.showing();
    expect(server.more().length, 'the return did not ask yet').toBe(asked);

    server.send(page([turn('t1', 'first')], null));
    expect(server.more().length, 'the held want fires on the settle').toBe(asked + 1);
    stop();
  });

  /**
   * A held return-want dies with the socket. The reconnect answers with an
   * ask of its own, so a want that outlived the drop fires a second ask at
   * the same core for the same page.
   */
  it('drops a held return-want when the socket goes', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'first')], null));

    chat.refresh();
    chat.leaving();
    chat.showing();
    // The socket drops before the in-flight ask settles: the want goes with
    // it, and the reconnect's own read is the ask a return gets.
    server.reach('closed');
    const asked = server.more().length;
    server.reach('open');
    server.send(page([turn('t1', 'first')], null));

    expect(server.more().length, 'the reconnect asked once, and the want did not ask again').toBe(
      asked + 1,
    );
    stop();
  });

  /**
   * A turn no page can put back survives the reach arm. A `forge_notice` line
   * lives in this conversation alone - the CLI wrote no row for it - so the
   * newest page a reconnect asks for carries no copy, and dropped with the
   * stale tail the row the reader was looking at is gone with no read that
   * can bring it back. The shown seat is the worst case: the reconnect's own
   * read is what empties it.
   */
  it('keeps the row the reader has when a reconnect re-serves around it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    // A notice on a seat with no turn yet opens its own row, which is its
    // only copy anywhere.
    server.update({ notice: { key: LEAD, severity: 'error', text: 'the plan is spent' } });

    // The socket drops and reconnects; the reconnect asks a newest page of
    // its own, and what lands shares no frame with what is held.
    server.reach('closed');
    server.reach('open');
    server.send(page([turn('t1', 'after the reconnect')], 'c1'));

    const drawn = words(chat);
    expect(drawn, 'the notice row is still drawn').toContain('the plan is spent');
    expect(drawn, 'and the newest page draws').toContain('after the reconnect');
    stop();
  });

  /**
   * The core's own line, which no transcript holds: the CLI never wrote a row
   * for it, so this store is the only place it can be drawn from.
   */
  it("draws the core's own line, at the severity it carries", () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ notice: { key: LEAD, severity: 'error', text: 'Usage: /mode <id>' } });

    const row = get(chat.value).turns.at(-1);
    expect(JSON.stringify(row?.messages), 'the line is drawn in the turn it arrived in').toContain(
      'Usage: /mode <id>',
    );
    expect(JSON.stringify(row?.messages), 'and it carries the severity it came with').toContain(
      'forge_notice',
    );

    // Nothing to say is nothing to draw: a malformed frame must not put an
    // empty row in front of the reader. Read off the ROWS, not their count -
    // a line that got through joins the turn it arrived in, which leaves the
    // count where it was.
    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    const before = drawn();
    server.update({ notice: { key: LEAD, severity: 'info', text: '' } });
    expect(drawn(), 'an empty line is not drawn').toBe(before);
  });

  /**
   * The review activity notice is the same line one origin over (#1776): a
   * worker's review turn ended, the core batched the tally into the
   * reviewer's session, and no transcript row holds it - the terminal draws
   * it as an info line and this store is the page's only place to draw one.
   */
  it('draws the review activity notice as a line of its own', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      review_activity_notice: {
        key: LEAD,
        branch: 'feat',
        waiting: 1,
        message: 'review #1 · 1 replied',
      },
    });

    const row = get(chat.value).turns.at(-1);
    expect(JSON.stringify(row?.messages), 'the notice text is drawn').toContain(
      'review #1 · 1 replied',
    );
    expect(JSON.stringify(row?.messages), 'as the core own line, at info').toContain(
      'forge_notice',
    );

    // A notice with no words is the same rule as the empty core line: nothing.
    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    const before = drawn();
    server.update({ review_activity_notice: { key: LEAD, branch: 'feat', waiting: 1 } });
    expect(drawn(), 'a wordless notice is not drawn').toBe(before);
  });

  /**
   * A connection failure draws its own line, not only the roster row's reason
   * (#1638): the terminal's answer, the raw why, or the rate-limit explainer
   * when the accounts are exhausted.
   */
  it('draws a connection failure, and the rate-limit explainer for one', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      connection_failed: {
        key: LEAD,
        message: 'connection to claude subprocess failed',
        fatal: true,
      },
    });

    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    expect(drawn(), 'the why is drawn').toContain(
      'Connection failed: connection to claude subprocess failed',
    );
    expect(drawn(), 'as the core own line').toContain('forge_notice');

    server.update({
      connection_failed: { key: LEAD, message: 'All accounts are exhausted', fatal: false },
    });
    expect(drawn(), 'a rate-limited failure draws the explainer instead').toContain(
      'Waiting for account reset; click another project or wait.',
    );
  });

  /**
   * A dispatch refused before it left the browser draws its line in the seat's
   * own column, where the click was made (#1638): the socket notes the loss
   * and this draws it, rather than the console alone.
   */
  it('draws a refused dispatch in its seat column, and not another seat one', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);

    refused({ org: LEAD.org, project: LEAD.project, label: 'somebody-else' });
    expect(drawn(), 'another seat refusal stays out of this column').not.toContain('Not sent');

    refused(LEAD);
    expect(drawn(), 'the line is drawn').toContain('Not sent - the connection is down.');
    expect(drawn(), 'as the core own line').toContain('forge_notice');
    expect(drawn(), 'a warning').toContain('warning');

    const once = drawn();
    refused(LEAD);
    expect(drawn(), 'the same line twice in a row is one row').toBe(once);
  });

  /**
   * The service status the core watches for the whole install arrives keyless,
   * so it rides every seat's stream and each open conversation draws it as the
   * terminal pushes it (#1638) - once per report.
   */
  it('draws a service-status report, once per report', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);

    // A seat-addressed update for ANOTHER seat must stay out: the door
    // passing keyless updates did not open it to other seats' news.
    server.update({
      connection_failed: {
        key: { org: 'OtherOrg', project: 'other', label: 'lead' },
        message: 'another seat failure',
        fatal: false,
      },
    });
    expect(drawn(), "another seat's failure stays out").not.toContain('another seat failure');

    server.update({
      service_status: { severity: 'warning', message: 'Elevated error rates on the Anthropic API' },
    });
    expect(drawn(), 'the report is drawn').toContain('Elevated error rates on the Anthropic API');
    expect(drawn(), 'as the core own line').toContain('forge_notice');
    expect(drawn(), 'a warning').toContain('"severity":"warning"');

    server.update({ service_status: { severity: 'error', message: 'The API is down' } });
    expect(drawn(), 'an error report draws as an error').toContain('"severity":"error"');
    expect(drawn()).toContain('The API is down');

    const once = drawn();
    server.update({ service_status: { severity: 'error', message: 'The API is down' } });
    expect(drawn(), 'the same report twice is one row').toBe(once);
  });

  /**
   * The core's fatal arrives keyless before the process goes (#1638): every
   * open conversation draws the line, in the terminal's own words.
   */
  it("draws the core's fatal once", () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);

    const fatal = {
      error: 'connection_failed',
      message: 'Failed to establish or maintain the Agent SDK bridge connection.',
    };
    server.update({ fatal_error: fatal });
    expect(drawn(), 'the fatal is drawn').toContain(
      'forge stopped: Failed to establish or maintain the Agent SDK bridge connection.',
    );
    expect(drawn(), 'as a failure').toContain('"severity":"error"');

    const once = drawn();
    server.update({ fatal_error: fatal });
    expect(drawn(), 'the same fatal twice is one row').toBe(once);
  });

  /**
   * The plan-limit next steps ride the turn's own failure (#1638): the
   * terminal's words and steps, with the core's own message where the
   * terminal's summary rides - it is not drawn a line above when a
   * dispatch-side refusal never reached the CLI.
   */
  it('adds the next steps to a plan-limited turn, with the core own words', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const notice = {
      key: LEAD,
      message: 'Usage limit reached',
      class: 'plan_limit',
      terminal_reason: null,
    };
    server.update({ turn_error: notice });

    const drawn = () => JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    expect(drawn(), 'the core own words ride the line').toContain('Usage limit reached');
    expect(drawn(), 'and the steps are drawn').toContain('Next steps');
    expect(drawn(), 'at the error the terminal draws them').toContain('"severity":"error"');

    // The same incident twice is one line, not two: the terminal upserts.
    const before = drawn();
    server.update({ turn_error: notice });
    expect(drawn(), 'a repeated incident keeps one line').toBe(before);
  });

  /**
   * A seat with no turn yet is the ordinary state, and a line that joins the
   * turn it arrived in has nothing to join there - so the core's own line is
   * the one `system` frame that opens a row. Held back, it would be dropped:
   * its only copy is the live frame.
   */
  it('draws the core line on a seat with no turn to join it to', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([], null));
    expect(get(chat.value).turns, 'precondition: nothing is drawn yet').toHaveLength(0);

    server.update({ notice: { key: LEAD, severity: 'error', text: 'Usage: /mode <id>' } });

    const [row] = get(chat.value).turns;
    expect(JSON.stringify(row), 'the line is drawn rather than dropped').toContain(
      'Usage: /mode <id>',
    );
    // **Not a turn being written.** A live row draws the running strip and its
    // clock, and the core's own header says no turn is in flight for a command
    // that ran none; nothing would clear the strip but a later page.
    expect(row?.live ?? false, 'the line is not a running turn').toBe(false);
  });

  /**
   * The other half of the same narrowing: with a turn to join, the line joins
   * it rather than opening a row of its own.
   */
  it('joins a turn the seat already has rather than opening a row', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ notice: { key: LEAD, severity: 'error', text: 'Usage: /mode <id>' } });

    expect(get(chat.value).turns, 'the row it joined is the one that was there').toHaveLength(1);
  });

  it('draws a mode or a model the CLI refused, which answers through no frame of its own', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      set_mode_failed: { key: LEAD, mode: 'plan', message: 'mode not permitted' },
    });
    const refusedMode = JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    expect(refusedMode, 'the refusal names what was asked for').toContain('plan');
    expect(refusedMode, 'and carries the CLI own words for it').toContain('mode not permitted');

    // The same arm carries a refused model, whose field is the other one: a
    // reader that read `mode` alone would draw "the session was refused".
    server.update({
      set_model_failed: { key: LEAD, model: 'sonnet', message: 'model not available' },
    });
    const refusedModel = JSON.stringify(get(chat.value).turns.at(-1)?.messages);
    expect(refusedModel, 'the refused model is named').toContain('sonnet');
    expect(refusedModel, 'with the CLI own words for that refusal').toContain(
      'model not available',
    );
  });

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
    chat.following(false);
    const before = get(chat.value);

    server.update({ chat_appended: { key: LEAD, msg: said('a line arriving') } });

    const after = get(chat.value);
    expect(after.following, 'the reader is still where they were, not pulled to the end').toBe(
      false,
    );
    expect(
      after.turns.slice(0, before.turns.length - 1),
      'and nothing above the turn the frame belongs to moved',
    ).toEqual(before.turns.slice(0, before.turns.length - 1));
  });

  /**
   * A delivered burst is one draw, not one per frame (#1670).
   *
   * **The drain a return delivers.** A page that was away comes back to every
   * frame that arrived meanwhile, and a store write each is a paint each -
   * the "frames moving very fast, filling up to the latest" of the report.
   * The fold still applies every frame (nothing is dropped, the held record
   * stays exact); only the draw waits for a painted frame, which is the split
   * the seat's own record already runs (`session/live.ts`'s `soon()`).
   */
  it('draws a burst of delivered frames once, not once per frame', async () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    let draws = 0;
    const stop = chat.value.subscribe(() => {
      draws += 1;
    });
    // The subscription's first call hands over the held value; the count
    // starts after it.
    draws = 0;

    // Five frames delivered back-to-back, the way one task of a resumed drain
    // delivers them.
    for (let n = 0; n < 5; n += 1) {
      server.update({ chat_appended: { key: LEAD, msg: said(`line ${n}`) } });
    }
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));

    // Exactly one: `< 5` would tolerate a partial replay, which is the shape
    // the defect had - several draws of the same burst's states.
    expect(draws, 'the burst is one painted state, not a replay').toBe(1);
    expect(
      JSON.stringify(get(chat.value)),
      'and nothing in it is dropped: the last frame is in the drawn record',
    ).toContain('line 4');
    stop();
  });

  /**
   * A paint that never comes must not stop the column.
   *
   * The publish waits for a painted frame (`soon()`), and the ONLY thing that
   * clears the flag is the callback itself - so a callback the browser drops
   * (a suspended page, a lock) would leave every later frame folded and
   * undrawn: the column stops ADDING rows while the rows already drawn stay
   * live, and only a read heals it. The watchdog publishes without the paint.
   */
  it('a paint that never comes does not stop the column', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    // Captured rather than swallowed, so a case can ALSO paint: the deadline
    // must be spent when the paint wins too, or a stale one publishes again.
    const frames = new Map<number, () => void>();
    let next = 1;
    const raf = vi.spyOn(window, 'requestAnimationFrame').mockImplementation((callback) => {
      const id = next;
      next += 1;
      frames.set(id, callback as () => void);
      return id;
    });
    const paint = (): void => {
      const waiting = [...frames.values()];
      frames.clear();
      for (const run of waiting) run();
    };
    try {
      const server = fakeConnection();
      const chat = new Chat(server.connection, LEAD);
      chat.start();
      server.send(page([turn('t1', 'first')], null));
      // Held, because a store nobody draws publishes at once by design - and
      // this case is the page that IS drawing. The drawn value is read off the
      // subscription rather than `get`, because attaching a reader flushes the
      // held state by design (`value`'s own subscribe) and would heal the very
      // wedge under test.
      let drawn = '';
      let publishes = 0;
      const stop = chat.value.subscribe((value) => {
        drawn = JSON.stringify(value);
        publishes += 1;
      });

      server.update({ chat_appended: { key: LEAD, msg: said('arrived while no paint came') } });
      expect(drawn, 'the frame is folded, and its draw waits for the paint').not.toContain(
        'arrived while no paint came',
      );

      vi.advanceTimersByTime(1_000);

      expect(drawn, 'the watchdog draws it without the paint').toContain(
        'arrived while no paint came',
      );

      // And the deadline is spent, not just fired: a watchdog that published
      // without clearing the flag would draw this one and re-wedge on the
      // very next frame.
      server.update({ chat_appended: { key: LEAD, msg: said('the frame after the deadline') } });
      vi.advanceTimersByTime(1_000);
      expect(drawn, 'and the next frame draws too').toContain('the frame after the deadline');

      // The other half of spending it: a paint that lands clears the deadline,
      // so the stale timer must not publish a second time behind it.
      server.update({ chat_appended: { key: LEAD, msg: said('painted, and no timer behind it') } });
      paint();
      expect(drawn, 'the paint draws it').toContain('painted, and no timer behind it');
      const settled = publishes;
      vi.advanceTimersByTime(1_000);
      expect(publishes, 'and the spent deadline publishes nothing more').toBe(settled);
      stop();
    } finally {
      raf.mockRestore();
    }
  });

  it("joins a skill's body to the turn whose Skill call loaded it, not a row of its own", () => {
    // The CLI injects the body as a user frame, and it can arrive above a
    // settled turn - where it opened a second row telling the same thing the
    // skill lane's call already tells. The store keeps it in that call's turn,
    // which is where the fold pairs the two.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(
      page(
        [
          {
            key: 't1',
            messages: [
              {
                type: 'assistant',
                uuid: 'a-skill',
                message: {
                  id: 'm-skill',
                  role: 'assistant',
                  model: 'claude-opus-5',
                  content: [
                    { type: 'tool_use', id: 'toolu_s', name: 'Skill', input: { skill: 'unslop' } },
                  ],
                },
              },
              ended(),
            ],
          },
        ],
        null,
      ),
    );

    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-body',
          message: {
            role: 'user',
            content: [
              {
                type: 'text',
                text: 'Base directory for this skill: /Users/ved/.claude/skills/unslop\n\n# Unslop\n\nEdit text.',
              },
            ],
          },
        },
      },
    });

    const turns = get(chat.value).turns;
    expect(turns, 'one turn, not a row beside it').toHaveLength(1);
    const units = fold(turns[0]?.messages ?? []);
    const [group] = units;
    const calls =
      group?.kind === 'leaves'
        ? group.rows.flatMap((row) => (row.tag === 'call' ? [row] : []))
        : [];
    expect(calls[0]?.leaf.skill, "the call's row is where the body landed").toBe(
      '# Unslop\n\nEdit text.',
    );
  });

  it("joins the harness's image line to the turn that read the picture", () => {
    // The line arrives as a user frame behind the result; on its own row it
    // draws as a separating notice of the reader's, where the agreed shape is
    // the call's own caption.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(
      page(
        [
          {
            key: 't1',
            messages: [
              {
                type: 'assistant',
                uuid: 'a-read',
                message: {
                  id: 'm-read',
                  role: 'assistant',
                  model: 'claude-opus-5',
                  content: [
                    {
                      type: 'tool_use',
                      id: 'toolu_shot',
                      name: 'Read',
                      input: { file_path: '/Users/ved/shot.png' },
                    },
                  ],
                },
              },
              {
                type: 'user',
                uuid: 'u-shot',
                message: {
                  role: 'user',
                  content: [
                    {
                      type: 'tool_result',
                      tool_use_id: 'toolu_shot',
                      content: [
                        {
                          type: 'image',
                          source: { type: 'base64', media_type: 'image/png', data: 'AAAA' },
                        },
                      ],
                    },
                  ],
                },
              },
            ],
          },
        ],
        null,
      ),
    );

    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-note',
          message: {
            role: 'user',
            content: [
              {
                type: 'text',
                text: '[Image: original 100x100, displayed at 100x100. Multiply coordinates by 1.00 to map to original image.]',
              },
            ],
          },
        },
      },
    });

    const turns = get(chat.value).turns;
    expect(turns, 'one turn, not a notice beside it').toHaveLength(1);
    const units = fold(turns[0]?.messages ?? []);
    expect(
      units.map((unit) => unit.kind),
      'and no row of its own',
    ).toEqual(['leaves']);
    const [group] = units;
    const calls =
      group?.kind === 'leaves'
        ? group.rows.flatMap((row) => (row.tag === 'call' ? [row] : []))
        : [];
    expect(calls[0]?.leaf.imageNote, "the call's row carries it").toContain('Multiply coordinates');
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
    // still opens none: a row it opened would draw nothing. It is HELD for
    // the turn that comes rather than dropped (rule 25).
    server.update({ chat_appended: { key: LEAD, msg: counted(1) } });
    expect(get(chat.value).turns, 'a frame with no turn to join opens no row').toEqual([]);

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
      // The counter that waited for a turn: the page was cut before it
      // arrived, so it rides the newest row the page brought.
      counted(1),
      counted(50),
      progressed(),
      counted(100),
    ]);
  });

  /**
   * A frame that waited for a turn rides the one a later frame opens.
   *
   * The counterpart of the page's newest row above, for a seat no page
   * answers: the counter of a turn whose opening frame is still coming
   * arrives first, so it waits and then leads the turn it was reported for
   * (rule 25 - nothing the seat sent is dropped for want of a turn).
   */
  it('a frame with no turn yet rides the turn the next frame opens', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    server.update({ chat_appended: { key: LEAD, msg: counted(7) } });
    expect(get(chat.value).turns, 'no row of its own while it waits').toEqual([]);

    server.update({ chat_appended: { key: LEAD, msg: said('the turn it belongs to') } });

    const turns = get(chat.value).turns;
    expect(turns, 'the opening frame made one turn').toHaveLength(1);
    expect(turns[0]?.messages, 'and the counter rode ahead of it').toEqual([
      counted(7),
      said('the turn it belongs to'),
    ]);

    // The wait ENDS with the turn that took it: a reset that never fired would
    // ride the spent counter into every later turn, the seen-twice shape this
    // whole mechanism exists to prevent.
    server.update({ chat_appended: { key: LEAD, msg: ended() } });
    server.update({ chat_appended: { key: LEAD, msg: typed('a second ask') } });

    const after = get(chat.value).turns;
    expect(after, 'the second ask opened its own turn').toHaveLength(2);
    expect(after[1]?.messages, 'and the spent counter did not ride again').toEqual([
      typed('a second ask'),
    ]);
  });

  /**
   * A page that already carries a waiting frame takes its place, once.
   *
   * The frame arrived while the page was in flight and the page's own copy
   * of it landed too: the two are one frame seen twice, so the page's copy
   * stands and the wait ends - drawing both is the doubled counter the
   * reconciliation exists to prevent.
   */
  it('a page that carries a waiting frame takes its place, drawn once', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    server.update({ chat_appended: { key: LEAD, msg: counted(3) } });
    server.send(page([{ key: 'p1', messages: [counted(3), said('the page had it')] }], null));

    const turns = get(chat.value).turns;
    expect(turns, 'the page landed as its own row').toHaveLength(1);
    expect(turns[0]?.messages, 'the frame is carried once, by the page').toEqual([
      counted(3),
      said('the page had it'),
    ]);
  });

  /**
   * An older page takes none of the frames waiting for a turn.
   *
   * They are newer than everything in it, so drawn there they would land in a
   * turn that ran before they were reported - and the wait outlives the page:
   * the next turn that opens still takes them.
   */
  it('leaves a waiting frame off an older page, and the next turn takes it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // An empty newest page - a cursor and no rows - so the frame has no turn
    // to join and the walk still has somewhere above to go. The fresh fill
    // asks for what is above it the moment it lands.
    server.update({ chat_appended: { key: LEAD, msg: counted(9) } });
    server.send(page([], '5'));
    expect(
      server.more().at(-1),
      'the empty page left somewhere to walk, and the fill asked for it',
    ).toEqual({ kind: 'more', conversation: LEAD, before: '5', turns: MORE_TURNS });

    server.send(page([turn('t0', 'older')], null));

    const held = get(chat.value).turns;
    expect(held, 'the older page drew its own row').toHaveLength(1);
    expect(
      JSON.stringify(held[0]?.messages),
      'and carries none of the frame that was waiting',
    ).not.toContain('thinking-9');

    // Still waiting, not spent: the next turn that opens takes it.
    server.update({ chat_appended: { key: LEAD, msg: typed('the next ask') } });
    expect(
      JSON.stringify(get(chat.value).turns.at(-1)?.messages),
      'and the next turn takes it',
    ).toContain('thinking-9');
  });

  /**
   * A ridden frame the page carries is not drawn twice.
   *
   * A ride parks a persisted frame in whatever row was there to take it, and
   * the page's own copy of the same frame can sit in a row the merge never
   * reconciles - a forge notice opens a row of its own on an empty seat, and
   * it is not live. The parked copy is let go when the page that owns the
   * frame lands, so the frame draws once, where the page put it.
   */
  it('heals a ridden frame the page carries, so it draws once', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // A persisted frame waits; a notice opens a row of its own and takes it.
    server.update({ chat_appended: { key: LEAD, msg: boundary() } });
    server.update({ notice: { key: LEAD, severity: 'info', text: 'Usage: /mode <id>' } });
    // The page lands carrying the frame's own copy in the turn it belongs to.
    server.send(page([{ key: 't1', messages: [boundary(), said('the real turn')] }], null));

    const carried = get(chat.value).turns.flatMap((row) => row.messages);
    expect(
      carried.filter((message) => JSON.stringify(message).includes('compact-1')).length,
      'the frame is carried once, by the page',
    ).toBe(1);
  });

  /**
   * A ridden frame never ties a live row to a page row that ran before it.
   *
   * The ride parks a persisted frame in the turn that opened next, and the
   * page can place that same frame in an OLDER turn of its own. Matched
   * through it, the live row would be replaced by the older turn account and
   * the frames that had joined it - a hook run no page copy carries - would
   * be lost, which is the drop rule 25 forbids.
   */
  it('does not let a ridden frame tie a live row to an older turn', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // The wait, the turn that takes it, and a hook run joining that turn.
    server.update({ chat_appended: { key: LEAD, msg: boundary() } });
    server.update({ chat_appended: { key: LEAD, msg: typed('the new ask') } });
    server.update({ chat_appended: { key: LEAD, msg: hookRun() } });

    // The page carries the frame's own older turn, then the new turn.
    server.send(
      page(
        [
          { key: 't-old', messages: [boundary(), said('the older turn')] },
          { key: 't-new', messages: [typed('the new ask'), said('the new answer')] },
        ],
        null,
      ),
    );

    const carried = get(chat.value).turns.flatMap((row) => row.messages);
    expect(
      carried.filter((message) => JSON.stringify(message).includes('hook-1-uuid')).length,
      'the hook run the page did not carry is still drawn',
    ).toBe(1);
  });

  /**
   * A parked frame the page does NOT carry stays where it was placed.
   *
   * The heal takes a parked copy back only when the page has its own: a frame
   * the page knows nothing about - a live-only counter - has no other copy,
   * and healing it away would be the drop rule 25 forbids.
   */
  it('leaves a parked frame the page does not carry where it was placed', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    server.update({ chat_appended: { key: LEAD, msg: counted(7) } });
    server.update({ chat_appended: { key: LEAD, msg: typed('the ask') } });
    server.send(page([turn('t1', 'an older row')], null));

    const carried = get(chat.value).turns.flatMap((row) => row.messages);
    expect(
      carried.filter((message) => JSON.stringify(message).includes('thinking-7')).length,
      'the parked counter survives a page that does not carry it',
    ).toBe(1);
  });

  /**
   * A page that took a waiting frame does not give it back to the next page.
   *
   * The first page is cut before the frame arrived, so it lands on that page's
   * newest row; the second repeats the row without the frame, and the wait is
   * over - placed again, the same counter would draw twice on one row.
   */
  it('a page that took a waiting frame does not give it back to the next page', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    server.update({ chat_appended: { key: LEAD, msg: counted(9) } });
    server.send(page([turn('t1', 'first')], '1'));
    server.send(page([turn('t1', 'first')], null));

    const carried = get(chat.value).turns.flatMap((row) => row.messages);
    expect(
      carried.filter((message) => JSON.stringify(message).includes('thinking-9')).length,
      'the taken frame is not placed a second time',
    ).toBe(1);
  });

  /**
   * A frame the PAGE ride placed is healed too, by the page that owns it.
   *
   * The first page was cut before the frame arrived and placed it on its
   * newest row; the next page carries the frame in the row it belongs to, so
   * the parked copy goes and the page's own is the one that draws.
   */
  it('heals a frame the page ride placed, when a later page carries it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    server.update({ chat_appended: { key: LEAD, msg: boundary() } });
    server.send(page([turn('t1', 'first')], '1'));
    server.send(page([{ key: 't-old', messages: [boundary(), said('the older turn')] }], null));

    const carried = get(chat.value).turns.flatMap((row) => row.messages);
    expect(
      carried.filter((message) => JSON.stringify(message).includes('compact-1')).length,
      'the frame draws once, where the page put it',
    ).toBe(1);
  });

  it('opens no row for a frame of any type the fold draws nothing out of', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first'), turn('t2', 'second')], null));

    // A tool result arrives as a `user` frame and draws nothing, on a running
    // seat it arrives between one turn and the next, so a row apiece is a blank
    // row per tool call.
    //
    // The dispatched frame draws nothing for a reason that is NOT its type -
    // the fold's own dispatch guard - so a rule keyed on types would open a row
    // for it.
    //
    // **A thinking block used to be one of these and is not any more**: it
    // draws the row its words are carried on, which is why it is not in this
    // set. **Nor is a monitor call**: it draws the plain call row every other
    // tool gets (rule 25), which the case below pins.
    server.update({ chat_appended: { key: LEAD, msg: result('call-1') } });
    server.update({ chat_appended: { key: LEAD, msg: dispatched() } });

    const after = get(chat.value).turns;
    expect(after.length, 'none of the frames opened a row of its own').toBe(2);
    expect(
      after[after.length - 1]?.messages,
      'and both are held in the turn they arrived in',
    ).toEqual([...turn('t2', 'second').messages, result('call-1'), dispatched()]);
  });

  it('opens a row for a monitor call, which draws one now', () => {
    // The converse of the rule above: a monitor is a call like any other, and
    // a skipped one is a style nobody knows is missing.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ chat_appended: { key: LEAD, msg: monitoring() } });

    const after = get(chat.value).turns;
    expect(after, 'the call opened a row of its own').toHaveLength(2);
    const units = fold(after[1]?.messages ?? []);
    const calls = units.flatMap((unit) =>
      unit.kind === 'leaves'
        ? unit.rows.flatMap((row) => (row.tag === 'call' ? [row.leaf] : []))
        : [],
    );
    expect(
      calls.map((leaf) => leaf.name),
      'and the row is the plain call row it is',
    ).toEqual(['Monitor']);
  });

  it('opens a row for a thinking frame, which draws one now', () => {
    // The converse of the rule above, and the reason the thinking came out of
    // its list: a frame that draws something is a row of its own, and a
    // thinking block carries words this page draws. Over a settled turn it
    // opens one.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'assistant',
          uuid: 'a-thought',
          message: {
            id: 'm-thought',
            role: 'assistant',
            model: 'claude-opus-5',
            content: [{ type: 'thinking', thinking: 'the model wondered' }],
          },
        },
      },
    });

    const after = get(chat.value).turns;
    expect(after.length, 'the thinking opened a row of its own').toBe(2);
    expect(after[1]?.live, 'and it is a live turn').toBe(true);
  });

  it('carries the reader own words into the turn the frames are writing', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // A frame that draws something opens the turn in flight when none is live.
    server.update({ chat_appended: { key: LEAD, msg: said('working') } });
    // Then the reader says something. The turn it interrupts is the one it
    // belongs to, so the words draw inside it rather than beside it.
    server.update({ chat_appended: { key: LEAD, msg: typed('now do this') } });

    const after = get(chat.value).turns;
    expect(after.length, 'the settled turn above, and the one being written').toBe(2);
    expect(after[1]?.messages, 'the turn kept the words rather than losing them to a row').toEqual([
      said('working'),
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

    // The page's copy is what the CLI persisted, which carries the id the CLI
    // minted for it - NOT the forged frame's absence of one. The two agree on
    // the assistant frame and on nothing else, which is what makes an id-keyed
    // match fail here rather than passing on `null === null`.
    server.send(
      page([{ key: null, messages: [minted('typed elsewhere'), said('answer-1'), ended()] }], '1'),
    );

    const after = get(chat.value).turns;
    expect(
      after.map((row) => row.key).includes(built),
      'the row the frames built is the one the page settled, not a second one',
    ).toBe(true);
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('typed elsewhere')).length,
      'and the turn is held once, as the page has it',
    ).toBe(1);
  });

  it('reconciles a forged row the page carries with no id on either copy', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // The delivery row is forged ONCE and held in the core, and the same row
    // still arrives as a frame - so the page carries what the stream has
    // already drawn, with no id on either copy. Frames are what the
    // reconciliation has by default, and an id-less row shares none, so the
    // prose is the only thing left to match on.
    server.update({ chat_appended: { key: LEAD, msg: forged('typed elsewhere') } });
    server.send(
      page([turn('t1', 'first'), { key: null, messages: [forged('typed elsewhere')] }], null),
    );

    const after = get(chat.value).turns;
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('typed elsewhere')).length,
      'the forged row is drawn once, not once per copy',
    ).toBe(1);
    expect(
      JSON.stringify(after).split('typed elsewhere').length - 1,
      'and the words sit in that row once, not once per copy of the frame',
    ).toBe(1);
  });

  it('finds an id-less frame inside a row that carries two user rows', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // The ordinary shape, as the fold emits it: a delivery rides a turn the
    // reader's own words opened, so one row carries two user rows. The frame
    // being reconciled is IN that row, and a comparison against the whole
    // row's words never equals one frame's - so both user rows read as not
    // carried and are added back to the row that already says them.
    const row = {
      key: null,
      messages: [forged('the reader asked'), said('an answer'), forged('a delivery')],
    };
    server.send(page([row], '2'));
    server.send(page([row], '2'));

    const after = get(chat.value).turns;
    expect(
      JSON.stringify(after).split('a delivery').length - 1,
      'the delivery is carried by the row, not added to it again',
    ).toBe(1);
    expect(
      JSON.stringify(after).split('the reader asked').length - 1,
      'and so is the reader own row',
    ).toBe(1);
  });

  it('keeps a live delivery the page ends on a settled one saying the same words', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // The page's last row is an OLDER delivery, settled, saying what the next
    // one says - which a repeating schedule makes ordinary. It is in reach of
    // the words fallback, but its exchange is over, and replacing the live row
    // with it drops the row the reader just received.
    const settled = {
      key: null,
      messages: [forged('check the build'), said('answered the old one'), ended()],
    };
    server.send(page([settled], '2'));

    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answered the new one') } });
    server.send(page([settled], null));

    const after = get(chat.value).turns;
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('answered the new one')).length,
      'the row the reader just received is still drawn',
    ).toBe(1);
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('answered the old one')).length,
      'beside the settled one it was confused with',
    ).toBe(1);
  });

  it('does not add an id-less frame a repeated row already carries', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // The core holds the delivery row, so a page carries it - and the next
    // page repeats that turn, because the server errs toward repeating a row
    // rather than toward a gap. Nothing about a frame with no id says which
    // copy it came from, so a repeat that reads an absent id as "not carried"
    // adds the same words to the row a second time.
    server.send(page([{ key: null, messages: [forged('check the build')] }], '2'));
    server.send(page([{ key: null, messages: [forged('check the build')] }], '2'));

    const after = get(chat.value).turns;
    expect(after, 'the repeat is one row').toHaveLength(1);
    expect(
      JSON.stringify(after).split('check the build').length - 1,
      'and its words are carried once, not once per page that repeats them',
    ).toBe(1);
  });

  it('keeps a live forged row apart from an older one saying the same words', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // A delivery that fired before, which says exactly what the next one will:
    // a repeating schedule is the ordinary case for identical prose.
    server.send(
      page(
        [{ key: null, messages: [forged('check the build'), said('answered the old one')] }],
        '2',
      ),
    );

    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answered the new one') } });
    // The page hands back the OLDER exchange, which is not the row being
    // written - so the live one has to survive as its own row. Prose alone
    // cannot tell the two apart, which is why a match is only taken against
    // the page's last row.
    server.send(
      page(
        [
          { key: null, messages: [forged('check the build'), said('answered the old one')] },
          { key: null, messages: [forged('a later one')] },
        ],
        null,
      ),
    );

    const after = get(chat.value).turns;
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('check the build')).length,
      'the older exchange and the live one are two rows',
    ).toBe(2);
    expect(
      after.filter((row) => JSON.stringify(row.messages).includes('answered the new one')).length,
      'and the live row keeps the frames the page was read too early to have',
    ).toBe(1);
  });

  it('takes a refusal as over when a page finally lands', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // Two turns, so the fresh fill asks nothing of its own and the reader's
    // walk is the ask this test is about.
    server.send(page([turn('t1', 'first'), turn('t1b', 'and then')], '2'));

    // The reader walks back while the seat's conversation is not held yet, so
    // the ask is refused - and the refusal is about THAT ask, not about the
    // conversation: its own words say asking again may find it. A refusal
    // that outlives its ask makes every later page undrawable.
    chat.older();
    server.refuse('more', 'the conversation is not held yet; asking again may find it');
    expect(get(chat.value).refused, 'the refusal is recorded').toBe(
      'the conversation is not held yet; asking again may find it',
    );

    chat.older();
    server.send(page([turn('t0', 'older')], null));

    const after = get(chat.value);
    expect(after.refused, 'and the ask it refused is over').toBeNull();
    expect(
      after.turns.map((row) => row.key),
      'with the page it waited for',
    ).toEqual(['t0', 't1', 't1b']);
  });

  it('asks a refused page again while the column is live, and stops once one lands', () => {
    vi.useFakeTimers();
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    expect(server.more(), 'the first ask').toHaveLength(1);

    // The core has not attached this conversation yet, and it refuses rather
    // than answering an empty page - an empty page reads as "nothing above"
    // and would make the history unreachable rather than late - and its own
    // words say asking again may find it.
    server.refuse('more', 'the conversation is not held yet; asking again may find it');
    expect(get(chat.value).refused, 'the refusal did not reach the column').not.toBeNull();

    // A beat later the column asks again by itself: a reader who stays put has
    // nothing else that would, and without this the words stand over a
    // conversation whose frames are landing while everything before the
    // refusal stays invisible.
    vi.advanceTimersByTime(2_000);
    expect(server.more(), 'the refused page was never asked again').toHaveLength(2);

    // This time the conversation is there, and the page answers the refusal.
    server.send(page([turn('t1', 'first')], '1'));
    expect(get(chat.value).refused, 'a landed page did not answer the refusal').toBeNull();

    // And a landed page ends the asking rather than being asked over.
    vi.advanceTimersByTime(10_000);
    expect(server.more(), 'a landed page kept being asked for').toHaveLength(2);
    stop();
  });

  it('stops asking a refused page once the column is stopped', () => {
    vi.useFakeTimers();
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();

    server.refuse('more', 'the conversation is not held yet; asking again may find it');
    stop();

    // Nothing draws the refusal any more, so nothing is owed an ask: a timer
    // left running here would poll a seat no page is showing.
    vi.advanceTimersByTime(30_000);
    expect(server.more(), 'a stopped column kept asking').toHaveLength(1);
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
    // The first page, the fresh fill it earned (one turn, with history above),
    // and the swap's own ask for the new occupant.
    expect(server.more().length, 'and it asks for the new occupant page').toBe(3);
  });

  /**
   * A frame waiting for a turn does not survive the occupant that reported it.
   *
   * The swap clears the held conversation, and the frames still waiting for a
   * turn are part of it: carried over, the last run's counter would ride into
   * the next occupant's first turn as a fact of a run that is not its own.
   */
  it('drops a waiting frame when the seat changes occupant', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();

    // Between the subscribe and its first page: no turn to join, so it waits.
    server.update({ chat_appended: { key: LEAD, msg: counted(5) } });
    server.update({ session_replaced: { key: LEAD, session_id: 'new-occupant' } });

    server.update({ chat_appended: { key: LEAD, msg: typed('the new occupant speaks') } });

    const fresh = get(chat.value).turns;
    expect(fresh, 'the new frame opened its own turn').toHaveLength(1);
    expect(fresh[0]?.messages, 'with none of the last occupant frames in it').toEqual([
      typed('the new occupant speaks'),
    ]);
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

  it('loads the new occupant when the abandoned ask is refused instead', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'the previous occupant')], '1'));

    chat.older();
    server.update({ session_replaced: { key: LEAD, session_id: 'new-occupant' } });
    // A refused ask is answered by no page at all: the count of abandoned asks
    // is spent on a page that is never coming, and the next page - the new
    // occupant's own - would be swallowed as if it were that answer.
    server.refuse('more', 'the conversation is gone');
    server.send(page([turn('n1', 'the new occupant')], null));

    expect(
      get(chat.value).turns.map((row) => JSON.stringify(row.messages)),
      'the new occupant page is drawn',
    ).toEqual([JSON.stringify(turn('n1', 'the new occupant').messages)]);
  });

  it('loads the new occupant after a socket drop took the abandoned ask', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'the previous occupant')], '1'));

    // An ask in flight, then the socket goes: the page it was waiting for dies
    // with it, which is what the reconnect's own ask exists to answer.
    chat.older();
    server.reach('closed');
    server.update({ session_replaced: { key: LEAD, session_id: 'new-occupant' } });
    server.reach('open');
    server.send(page([turn('n1', 'the new occupant')], null));

    expect(
      get(chat.value).turns.map((row) => JSON.stringify(row.messages)),
      'the reconnect answer is drawn, not swallowed as the dead ask reply',
    ).toEqual([JSON.stringify(turn('n1', 'the new occupant').messages)]);
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
    expect(
      after.map((row) => row.key).includes(built),
      'the row the frames built is the one the page settled, not a second one',
    ).toBe(true);
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
    // The page still carries the turn above it, as every page does - whole
    // turns, contiguous - so the merge reaches what is held.
    server.send(page([turn('t1', 'first'), { key: null, messages: [typed('mine')] }], '1'));
    const held = get(chat.value).turns.at(-1)?.key ?? '';
    // The answer then arrives as frames, opening a live turn over that row.
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-2') } });

    // The next page repeats that row, and its own copy carries the frames. A
    // repeated row is handed back as the object already held, whose messages
    // are older - asking THAT object is asking the wrong copy.
    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: null, messages: [typed('mine'), said('answer-1'), said('answer-2'), ended()] },
        ],
        '1',
      ),
    );

    const rows = get(chat.value).turns.map((row) => JSON.stringify(row.messages));
    expect(
      rows.filter((messages) => messages.includes('answer-2')).length,
      'the turn is held once, not as a stale copy beside a live one',
    ).toBe(1);
    expect(rows.length, 'and no extra row survives it').toBe(2);
    expect(held, 'the row kept the name the page gave it').toBe('turn-u-mine');
  });

  it('keeps a call and the frames that update it in one turn', () => {
    // The live path's own half of #1322, and #1359's rule is what holds it: a
    // frame the fold draws nothing out of is not a row, so a call's own result
    // - which arrives in a user frame - joins the turn the call is in rather
    // than opening one. Without that, the task frames land in a turn of their
    // own, where an update naming only its task can never find its call, and a
    // backgrounded command draws as finished for the rest of the session.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const launch = {
      type: 'assistant',
      message: {
        id: 'm-launch',
        role: 'assistant',
        model: 'claude-opus-5',
        content: [
          {
            type: 'tool_use',
            id: 'toolu_01Bg',
            name: 'Bash',
            input: { command: 'sleep 30', run_in_background: true },
          },
        ],
      },
    };
    const started = {
      type: 'system',
      subtype: 'task_started',
      task_id: 'bj5g0t2kq',
      tool_use_id: 'toolu_01Bg',
      uuid: 'task-1',
    };

    server.update({ chat_appended: { key: LEAD, msg: launch } });
    server.update({ chat_appended: { key: LEAD, msg: result('toolu_01Bg') } });
    server.update({ chat_appended: { key: LEAD, msg: started } });

    const after = get(chat.value).turns;
    expect(after.length, 'the result opened no turn of its own').toBe(2);
    expect(after[after.length - 1]?.messages, 'and is held with the call it answers').toEqual([
      launch,
      result('toolu_01Bg'),
      started,
    ]);
  });

  it('draws a peer message the socket sends as a forged frame', () => {
    // #1376: the server forges the frame a delivery needs and sends it beside
    // the typed update, so the client draws a peer message with the
    // `chat_appended` it already handles rather than with an arm of its own.
    // **What this pins is the seam between the two**: the frame carries the
    // envelope PROSE, and the fold is what reads it back as traffic rather than
    // as the reader's own words.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    const envelope = {
      type: 'user',
      uuid: 'u-envelope',
      message: {
        role: 'user',
        content: [
          {
            type: 'text',
            text: "[Message id=t-9c1 from agent 'forge/steward' (org 'Busytools')]\n\npicking it up",
          },
        ],
      },
    };
    server.update({ chat_appended: { key: LEAD, msg: envelope } });

    const held = get(chat.value).turns.at(-1)?.messages ?? [];
    expect(
      fold(held, LEAD).map((unit) => unit.kind),
      'the forged frame draws as traffic, not as the reader own turn',
    ).toEqual(['leaves']);
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

  it('draws no two rows under one key when a page repeats an exchange the words arm can reach', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // The delivery opens a live turn, so its name is the ordinal and its
    // opener carries no id.
    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });

    // A page read while that turn is in flight, whose last row is the ordinary
    // two-user-row shape. The claim lands on the words, and the live turn's
    // messages become the page's copy - so its opener now carries an id while
    // its name stays the one it was opened with.
    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: null, messages: [typed('mine'), said('answer-1'), forged('check the build')] },
        ],
        null,
      ),
    );

    // A later page: the settled account of that exchange, and then a fresh row
    // repeating the same words. The repeat is an exchange of its own, and a
    // claim landing on it too draws both rows under the live turn's key, which
    // the list throws on.
    server.send(
      page(
        [
          turn('t1', 'first'),
          {
            key: null,
            messages: [typed('mine'), said('answer-1'), forged('check the build'), ended()],
          },
          { key: null, messages: [forged('check the build')] },
        ],
        null,
      ),
    );

    const after = get(chat.value).turns;
    const keys = after.map((row) => row.key);
    expect(new Set(keys).size, 'no two rows are drawn under one key').toBe(keys.length);
    expect(after, 'and the repeat keeps a row of its own beside the exchange').toHaveLength(3);
  });

  it('lets one row of a page take a live turn, not two', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // A delivery with no id opens a live turn, and the answer joins it - so the
    // opener carries no id and the words arm stays in reach.
    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });

    // The page carries the settled account of that exchange - which shares the
    // answer frame - and, last, a fresh delivery repeating the words. The
    // account is what the live turn is, and the repeat must not take it as
    // well: two rows under one key is what the list throws on.
    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: null, messages: [minted('check the build'), said('answer-1'), ended()] },
          { key: null, messages: [forged('check the build')] },
        ],
        null,
      ),
    );

    const keys = get(chat.value).turns.map((row) => row.key);
    expect(new Set(keys).size, 'no two rows are drawn under one key').toBe(keys.length);
  });

  it('keeps a repeat from taking a live turn an id-bearing frame opened', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // A delivery, its answer, and a page read while the turn was in flight: the
    // claim lands and the live turn's messages become the page's copy, so its
    // opener now carries an id and its name stays the one it was opened with.
    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });
    server.update({ chat_appended: { key: LEAD, msg: said('answer-1') } });
    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: null, messages: [typed('mine'), said('answer-1'), forged('check the build')] },
        ],
        null,
      ),
    );
    const exchange = get(chat.value).turns.at(-1)?.key ?? '';

    // The repeat: another delivery of the same words, which opens a live turn
    // of its own, and the page that hands it back without reaching back as far
    // as the exchange. A turn an id-bearing frame opened is reconciled by ids,
    // so the repeat finds its own live turn rather than the exchange - which
    // would take the exchange's row and hand it the repeat's opening frame.
    server.update({ chat_appended: { key: LEAD, msg: forged('check the build') } });
    server.send(
      page([turn('t1', 'first'), { key: null, messages: [forged('check the build')] }], null),
    );

    const after = get(chat.value).turns;
    expect(
      after.find((row) => row.key === exchange)?.messages[0],
      'the exchange row still opens on its own frame',
    ).toEqual(typed('mine'));
  });
});

describe('the chat holds a queued prompt until the CLI takes it', () => {
  afterEach(() => {
    // The pending send outlives the store, so a case that leaves one behind
    // would hand it to the next.
    echoes.clear(subjectKey({ session: LEAD }));
  });

  it('holds the forged row while the pile is drawing it, and drains it at started', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    // The core announces the queue and forges the user turn beside it, in
    // that order: the pile draws the card, and the chat holds its copy.
    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'hold me' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('hold me', 'p1') } });
    expect(words(chat), 'the card is the only thing drawing it').not.toContain('hold me');

    // A state this build does not act on keeps the hold: dropping on a parse
    // miss is the one failure nobody can see.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'queued' } });
    expect(words(chat), 'queued keeps the wait').not.toContain('hold me');

    // `started` is the drain, and the wait is written on the row.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'the drain draws the words').toContain('hold me');
    expect(words(chat), 'an instant wait is sent, said plainly').toContain('"forge_note":"sent"');

    // The lifecycle repeats (a second frame for the same id) and the row is
    // not drawn twice.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'completed' } });
    expect(words(chat).split('hold me').length - 1, 'one row, once').toBe(1);
  });

  it('retracts a row the announcement follows, which is the real wire order', () => {
    // The dispatcher emits the user turn as it routes; the task's announcement
    // comes a beat behind, so the frame usually arrives first. The hold has to
    // come from that side of the race too, or a live send is drawn by the chat
    // and the pile at once.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('raced words', 'p1') } });
    expect(words(chat), 'the frame lands first and is drawn').toContain('raced words');

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'raced words' } });
    expect(words(chat), 'the announcement pulls it back into the hold').not.toContain(
      'raced words',
    );

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'and the drain draws it once, when it starts').toContain('raced words');
    expect(words(chat).split('raced words').length - 1, 'once').toBe(1);
  });

  it('holds the row a page carries while the prompt still waits', () => {
    // The transport keeps the forged turn it was sent, so a read taken while
    // the prompt waits hands the words back as conversation - and a reader
    // attaching mid-queue would meet the card and the row at once.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'page-carried' },
    });
    server.send(
      page(
        [turn('t1', 'first'), { key: 't2', messages: [forgedUnder('page-carried', 'p1')] }],
        null,
      ),
    );
    expect(words(chat), 'the card is the only drawing, page or not').not.toContain('page-carried');

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'the drain draws it').toContain('page-carried');
    expect(words(chat).split('page-carried').length - 1, 'once').toBe(1);
  });

  it('settles the pending mark when its own prompt reaches the queue, and only its own', () => {
    // **The card is what carries a waiting prompt's words.** The mark stands
    // in for the row the words will occupy - and the pile's card IS that row
    // while the prompt waits, so the mark left up would draw the words twice:
    // once in the pile, once in the chat saying "sending" (Ved's live find,
    // 2026-10-04). Keyed on the id, like the cancel: two sends of the same
    // text compare equal by words.
    const seatKey = subjectKey({ session: LEAD });
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    echoes.post(seatKey, 'queue words', true, 'p9');
    server.update({ prompt_queued: { key: LEAD, uuid: 'p-other', source: 'you', text: 'x' } });
    expect(echoes.of(seatKey)?.id, "another prompt's queueing is not this send's to settle").toBe(
      'p9',
    );

    server.update({ prompt_queued: { key: LEAD, uuid: 'p9', source: 'you', text: 'queue words' } });
    expect(
      echoes.of(seatKey),
      'the card carries them now, so the mark goes with it',
    ).toBeUndefined();
  });

  it('settles the pending mark when its own prompt is cancelled, and only its own', () => {
    // The cancelled arm settles a mark the queue arm has not: a cancel that
    // beats its own queueing frame, or one for a send the core took another
    // way - so a regression here leaves "sending" up forever. And it keys on
    // the ID: two sends of the same text compare equal by words, so the words
    // cannot be what separates them.
    const seatKey = subjectKey({ session: LEAD });
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    echoes.post(seatKey, 'same words', true, 'e-other');
    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'same words' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('same words', 'p1') } });
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'cancelled' } });
    expect(
      echoes.of(seatKey)?.id,
      "a second send of the same words is not this cancel's to settle",
    ).toBe('e-other');

    echoes.post(seatKey, 'other words', true, 'p2');
    server.update({ prompt_queued: { key: LEAD, uuid: 'p2', source: 'you', text: 'other words' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('other words', 'p2') } });
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p2', state: 'cancelled' } });
    expect(echoes.of(seatKey), "the prompt's own mark goes with it").toBeUndefined();
  });

  it('holds a forged row a page joins frames to, which is the ordinary shape', () => {
    // The server's fold opens a turn AT the forged user row and the frames
    // that follow join that span - so the row never stands alone, and a
    // quieting that only spliced when a row EMPTIED would keep the copy
    // beside the card and drain it a second time.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'partial words' },
    });
    server.send(
      page(
        [
          turn('t1', 'first'),
          {
            key: 't2',
            messages: [forgedUnder('partial words', 'p1'), said('answer in between')],
          },
        ],
        null,
      ),
    );
    expect(words(chat), 'the card is the only drawing of the words').not.toContain('partial words');
    expect(words(chat), 'while the frames around them still draw').toContain('answer in between');

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat).split('partial words').length - 1, 'the drain draws it once').toBe(1);
    expect(words(chat), 'with the wait on it').toContain('"forge_note":"sent"');
  });

  it("arms from the read's own queue, which is all a fresh attacher is handed", () => {
    // A socket client attaching mid-queue gets no backlog of `prompt_queued`
    // frames - the pending backlog goes to the first-ever subscriber alone -
    // so the snapshot's queue is the only word this reader ever gets, and the
    // card deliberately comes from the read for exactly this reader.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: {
        header: { turn_in_flight: true },
        state: { queue: [{ uuid: 'p1', source: 'you', text: 'fresh words' }] },
      },
    });
    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: 't2', messages: [forgedUnder('fresh words', 'p1'), said('answer in between')] },
        ],
        null,
      ),
    );
    expect(words(chat), 'no announcement ever came, and the row is still held').not.toContain(
      'fresh words',
    );

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat).split('fresh words').length - 1, 'the drain draws it once').toBe(1);
    expect(words(chat), 'with the wait on it').toContain('"forge_note":"sent"');
  });

  it('a read asked before the send answers after it, and the hold stays', () => {
    // **The read is a round trip: its listing is taken when the server
    // answers, not when the client asked.** A snapshot requested a beat
    // before a send carries a queue that PREDATES the prompt - releasing on
    // it would draw the row the pile is holding, and no later snapshot comes
    // until the next re-read.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    // The store's own first snapshot, which every subscribe is answered with.
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: false }, state: { queue: [] } },
    });
    server.send(page([turn('t1', 'first')], null));

    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('stale racing words', 'p1') } });
    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'stale racing words' },
    });
    expect(words(chat), 'held while the queue lists it').not.toContain('stale racing words');

    // The read's answer, whose queue was snapshotted before the prompt.
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: true }, state: { queue: [] } },
    });
    expect(words(chat), 'an older listing must not draw the row').not.toContain(
      'stale racing words',
    );

    // And the lifecycle still drains it when the CLI takes the prompt.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'the drain draws it').toContain('stale racing words');
  });

  it('releases a hold the read no longer lists, so a drop cannot strand the words', () => {
    // Queued, then the socket drops: the prompt settles during the gap and
    // its lifecycle frames die on the dead connection. The reconnect's
    // snapshot is the first word after it, and the invariant is that the
    // hold never outlives the queue's listing of the id - a uuid it no
    // longer lists has settled, so the words DRAW rather than being filtered
    // from every surface forever with the card gone too.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'lost words' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('lost words', 'p1') } });
    expect(words(chat), 'held while the queue lists it').not.toContain('lost words');

    server.reach('closed');
    server.reach('open');
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: false }, state: { queue: [] } },
    });
    expect(words(chat), 'the read no longer lists it, so it draws').toContain('lost words');

    server.send(
      page([turn('t1', 'first'), { key: 't2', messages: [forgedUnder('lost words', 'p1')] }], null),
    );
    expect(words(chat).split('lost words').length - 1, 'and the page pairs with it, once').toBe(1);
  });

  it('pulls back a copy drawn before the snapshot armed, which is the page-first order', () => {
    // A cold load asks `more` before it re-subscribes, so the page can land
    // FIRST - with the forged row unfiltered, because nothing had armed yet.
    // The snapshot then lists the uuid and must retract that copy, or the
    // words stand in both surfaces for the whole wait.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.send(
      page(
        [
          turn('t1', 'first'),
          { key: 't2', messages: [forgedUnder('early words', 'p1'), said('answer in between')] },
        ],
        null,
      ),
    );
    expect(words(chat), 'the page landed first, unfiltered').toContain('early words');

    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: {
        header: { turn_in_flight: true },
        state: { queue: [{ uuid: 'p1', source: 'you', text: 'early words' }] },
      },
    });
    expect(words(chat), 'the read claims it, and the drawn copy is pulled back').not.toContain(
      'early words',
    );
    expect(words(chat), 'while the frames around it stay').toContain('answer in between');

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat).split('early words').length - 1, 'the drain draws it once').toBe(1);
    expect(words(chat), 'with the wait on it').toContain('"forge_note":"sent"');
  });

  it('settles the pending mark on the id even when the hold has been released', () => {
    // The cancel is the mark's only settler, and it must survive a hold
    // cleared out from under it: a snapshot release (a drop mid-queue)
    // between the send and the cancel would otherwise leave "sending" up
    // forever.
    const seatKey = subjectKey({ session: LEAD });
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    echoes.post(seatKey, 'orphan words', true, 'p1');
    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'orphan words' },
    });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('orphan words', 'p1') } });
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: true }, state: { queue: [] } },
    });

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'cancelled' } });
    expect(echoes.of(seatKey), 'the id is the whole test, held row or not').toBeUndefined();
  });

  it('draws the frame the last pull named, which is what the hold keeps', () => {
    // The two pulls do not order themselves - a page can land before the
    // frame or after it - and whichever names the uuid LAST holds the copy
    // that draws at the drain. The words are the same either way; only the
    // frame differs, so the choice is pinned rather than left to drift back
    // behind a keep-first guard nothing measures.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'either words' },
    });
    server.send(
      page(
        [
          turn('t1', 'first'),
          {
            key: 't2',
            messages: [
              {
                type: 'user',
                uuid: 'p1',
                message: { role: 'user', content: [{ type: 'text', text: 'either words' }] },
                forge_marker: 'page',
              },
            ],
          },
        ],
        null,
      ),
    );
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'p1',
          message: { role: 'user', content: [{ type: 'text', text: 'either words' }] },
          forge_marker: 'live',
        },
      },
    });

    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'the last pull holds the frame that draws').toContain(
      '"forge_marker":"live"',
    );
    expect(words(chat).split('either words').length - 1, 'and it draws once').toBe(1);
  });

  it('writes the wait the row spent in the pile, in the pile vocabulary', () => {
    vi.useFakeTimers();
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'cron', text: 'wait for it' },
    });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('wait for it', 'p1') } });
    vi.advanceTimersByTime(4_000);
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });

    expect(words(chat), 'four seconds of waiting, and the drain').toContain(
      'queued 0:04 \u{b7} sent',
    );
  });

  it('draws refused words without the note, and drops cancelled ones', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'refuse me' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('refuse me', 'p1') } });
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'refused' } });
    expect(words(chat), 'a hook-refused prompt still draws its words').toContain('refuse me');
    expect(words(chat), 'and says nothing it cannot know').not.toContain('forge_note');

    server.update({ prompt_queued: { key: LEAD, uuid: 'p2', source: 'you', text: 'drop me' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('drop me', 'p2') } });
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p2', state: 'cancelled' } });
    expect(words(chat), 'a cancelled prompt goes with its card').not.toContain('drop me');
  });

  it('releases a held row when the process that would start it fails', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'doomed' } });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('doomed', 'p1') } });
    server.update({
      connection_failed: { key: LEAD, message: 'the process exited', fatal: false },
    });

    // A later lifecycle for the dead occupant's id draws nothing: the wait
    // went with the process that held it.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'no start is owed to a dead occupant').not.toContain('doomed');
  });

  /**
   * The pulled prompt's row stays mounted, keyed, across the hold.
   *
   * Removed (the old shape), the list's virtualiser splices the last row's
   * size away and the drain re-adds it uncached - drawn at the estimate for
   * one frame, which is the chat-wide up-and-down on every Enter (#1890,
   * confirmed live 2026-10-09). Kept, the size survives and the words land
   * back in the row they left.
   */
  it('keeps a pulled prompt row mounted, so the list cannot flash', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'kept')], null));

    // The row arrives before the pile hears about the queue (the race the
    // retraction exists for): it opens the live turn...
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('the words', 'p1') } });
    const opened = get(chat.value).turns;
    const key = opened[opened.length - 1]?.key;
    expect(key, 'the row opened a turn').toBeDefined();
    expect(JSON.stringify(opened), 'precondition: the words drew').toContain('the words');

    // ...and the queue's announcement pulls them into the card - the row must
    // STAY, under the same key, drawing nothing.
    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'the words' } });
    const held = get(chat.value).turns;
    expect(held[held.length - 1]?.key, 'the row keeps its key').toBe(key);
    expect(held[held.length - 1]?.held, 'and is marked held').toBe(true);
    expect(JSON.stringify(held), 'and draws nothing while it waits').not.toContain('the words');

    // The drain fills the row in place: the same key, the hold over, the words
    // back.
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    const drained = get(chat.value).turns;
    expect(drained[drained.length - 1]?.key, 'the drain lands in the same row').toBe(key);
    expect(drained[drained.length - 1]?.held, 'and the hold is over').toBe(false);
    expect(JSON.stringify(drained), 'with the words back in it').toContain('the words');
    stop();
  });

  /**
   * A reconnect that takes the queue with it must not leave a held row standing
   * empty: clearing the hold bookkeeping without the row strands it, no drain
   * can fill it, and the page that later carries the prompt draws it again.
   */
  it('releases a held row when a reconnect takes the queue that held it', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'kept')], null));

    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('the words', 'p1') } });
    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'the words' } });
    expect(get(chat.value).turns.at(-1)?.held, 'precondition: the row is held').toBe(true);

    // The process is gone, and its queue with it.
    server.update({ connected: { key: LEAD, session_id: 's-2', cwd: '/tmp' } });

    expect(
      get(chat.value).turns.some((row) => row.held === true),
      'no row is left held',
    ).toBe(false);

    // And a page carrying the same prompt draws it once, not beside a blank row.
    server.send(
      page([turn('t1', 'kept'), { key: null, messages: [forgedUnder('the words', 'p1')] }], null),
    );
    const rows = get(chat.value).turns.filter((row) =>
      JSON.stringify(row.messages).includes('the words'),
    );
    expect(rows, 'the words draw in one row').toHaveLength(1);
    stop();
  });

  it('pairs the drained row with the page copy carried by its attachment', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({
      prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'mid-turn words' },
    });
    server.update({ chat_appended: { key: LEAD, msg: forgedUnder('mid-turn words', 'p1') } });
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    expect(words(chat), 'precondition: the drain drew it').toContain('mid-turn words');

    // The page's own copy of a mid-turn prompt is its attachment row, whose top
    // uuid is the CLI's while the prompt's id rides `source_uuid`. A copy that
    // matched on the top uuid would land as a SECOND row for one prompt.
    server.send(
      page(
        [
          turn('t1', 'first'),
          {
            key: null,
            messages: [
              {
                type: 'user',
                uuid: 'cli-row-1',
                message: {
                  role: 'user',
                  content: [
                    {
                      type: 'queued_command',
                      prompt: 'mid-turn words',
                      source_uuid: 'p1',
                    },
                  ],
                },
              },
            ],
          },
        ],
        null,
      ),
    );

    expect(words(chat).split('mid-turn words').length - 1, 'one prompt, one row').toBe(1);
  });

  /**
   * The queue's own frames fold while the seat is away: the lifecycle that
   * lets a held prompt go is what no read repeats - dropped, the return's
   * page re-withheld the row its own started frame had released, and the
   * words appeared nowhere (the echo draws nothing for a queued send).
   */
  it('lets a held prompt go when its start lands while the seat is away', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'held words' } });
    chat.leaving();
    server.update({ prompt_lifecycle: { key: LEAD, uuid: 'p1', state: 'started' } });
    chat.showing();
    server.send(
      page([turn('t1', 'first'), { key: 't2', messages: [forgedUnder('held words', 'p1')] }], null),
    );

    expect(words(chat), 'the row draws on the return').toContain('held words');
    stop();
  });

  /**
   * A connect that lands while the seat is away releases the queue hold: the
   * process is gone and its queue with it, and a connect is that fact from
   * the other side. It is home news, so it reaches an away seat - dropped,
   * the hold outlives the queue and the return's page re-withholds a row
   * nothing will ever release.
   */
  it('lets a connect that lands while the seat is away release the queue hold', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();
    server.send(page([turn('t1', 'first')], null));

    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'held words' } });
    chat.leaving();
    server.update({ connected: { key: LEAD, session_id: 's-2', cwd: '/tmp' } });
    chat.showing();
    server.send(
      page([turn('t1', 'first'), { key: 't2', messages: [forgedUnder('held words', 'p1')] }], null),
    );

    expect(words(chat), 'the row draws on the return').toContain('held words');
    stop();
  });
});

describe('the answer a call-output read leaves behind', () => {
  afterEach(() => {
    outputs.clear(subjectKey({ session: LEAD }));
  });

  /**
   * The answer is kept where the row that asked reads it - by the call's own
   * id - and a swap takes the OUTGOING occupant's answers with it: they are
   * about calls the new occupant never made.
   */
  it('keeps the answer for its call, and drops it with the occupant', () => {
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();

    server.update({ call_output: { key: LEAD, call_id: 'tu-1', output: { lines: ['one'] } } });
    expect(outputs.of('tu-1'), 'the answer is readable by the id it was asked with').toEqual({
      kind: 'lines',
      lines: ['one'],
    });

    server.update({ session_replaced: { key: LEAD } });
    expect(outputs.of('tu-1'), "the going occupant's answers go with it").toBeUndefined();
    stop();
  });

  it('draws an answer it cannot name rather than reading it as nothing', () => {
    // Rule 25 at the seam: a shape this build does not know is kept as
    // `unknown` and drawn plainly, so a future answer cannot reach the row
    // as a blank.
    const server = fakeConnection();
    const chat = new Chat(server.connection, LEAD);
    const stop = chat.start();

    server.update({ call_output: { key: LEAD, call_id: 'tu-2', output: { novel: true } } });
    expect(outputs.of('tu-2')).toEqual({ kind: 'unknown' });
    stop();
  });
});

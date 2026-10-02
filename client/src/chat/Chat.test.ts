// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Chat as HandedChat, type Conversation } from './conversation';
import { freeze } from './testing/frozen';
import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import Chat from './Chat.svelte';
import { installResizeObserver } from './testing/viewport';

installResizeObserver();

/**
 * The list is a stub, as it is for the scroll tests: what has to be seen here
 * is what the column hands the rows, and `virtua` measures through APIs jsdom
 * does not implement.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

vi.mock('./conversation', async (importOriginal) => {
  const { frozenConversation } = await import('./testing/frozen');
  return frozenConversation(await importOriginal<typeof import('./conversation')>());
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

/** One assistant frame, whose usage the running row draws. */
const frame = (uuid: string, input: number): unknown => ({
  type: 'assistant',
  uuid,
  timestamp: '2026-10-01T06:00:00Z',
  message: {
    id: `m-${uuid}`,
    role: 'assistant',
    model: 'claude-opus-5',
    content: [{ type: 'text', text: 'working' }],
    usage: { input_tokens: input, output_tokens: 20 },
  },
});

/** The frame a turn ends on, which is the only carrier of a session cost. */
const settledFrame = (uuid = 'r1', total = 12.5): unknown => ({
  type: 'result',
  uuid,
  duration_ms: 42_000,
  duration_api_ms: 20_000,
  total_cost_usd: total,
  usage: {},
});

/** A frame arriving on the seat, which is how a turn opens. */
const appended = (msg: unknown): ServerMessage => ({
  kind: 'update',
  update: { chat_appended: { key: LEAD, msg } },
});

/** The core saying the turn is over, which is what asks for the page that settles it. */
const ended = (): ServerMessage => ({ kind: 'update', update: { turn_complete: { key: LEAD } } });

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

  it('keeps the conversation drawn when a page is refused', () => {
    // **A refusal is about an ASK, not about the conversation.** A page asked
    // for before the seat's conversation is held is refused with words that
    // say asking again may find it - so a column that replaced the turns with
    // the refusal would take the reader's own history away, unmount the list
    // and leave nothing able to ask again.
    const say = (text: string): unknown => ({
      type: 'assistant',
      message: {
        id: `m-${text}`,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text }],
      },
    });
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [say('the first answer')] }]);
    flushSync();

    server.send({ kind: 'error', what: 'more', why: 'the conversation is not held yet' });

    expect(drawn(), 'the turn the reader was reading is still drawn').toContain('the first answer');
    expect(drawn(), 'and the refusal is said beside it').toContain(
      'the conversation is not held yet',
    );
  });

  it('keeps a row the reader opened when a page prefixes the turn with their words', () => {
    // The shape a dropped socket leaves: the turn ran while the client was
    // away, so its call arrived as frames and the reader's own words never did.
    // The page answering the reconnect carries the prompt as well, so the copy
    // of that turn BEGINS with a `mine` block - every position below it shifts,
    // and a list keyed by position remounts the call the reader had open.
    const frame = (uuid: string, content: unknown[]): unknown => ({
      type: 'assistant',
      uuid,
      message: { id: `m-${uuid}`, role: 'assistant', model: 'claude-opus-5', content },
    });
    const said = (text: string): unknown => ({
      type: 'user',
      uuid: `u-${text}`,
      message: { role: 'user', content: [{ type: 'text', text }] },
    });
    const call = frame('a1', [
      { type: 'tool_use', id: 'toolu_r1', name: 'Read', input: { file_path: 'src/lib.rs' } },
    ]);

    const server = stub();
    draw({}, server);
    server.answer([]);
    server.send({ kind: 'update', update: { chat_appended: { key: LEAD, msg: call } } });

    const row = document.querySelector<HTMLDetailsElement>('details.leaf');
    expect(row, 'the call drew as a row that opens').not.toBeNull();
    if (row !== null) row.open = true;

    server.answer([{ key: null, messages: [said('go'), call] }]);
    flushSync();

    expect(row?.isConnected, 'the row the reader opened is the row still on the page').toBe(true);
    expect(row?.open, 'and it is still open').toBe(true);
    expect(drawn(), 'and the reader own words drew above it').toContain('go');
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

  it('draws the compaction line on a column that has no turn to hang it on', () => {
    // Every state the column can be in has a rendering, and this is the one
    // state where the line has no turn to belong to: a compaction running
    // before the first page lands, or on a seat that has said nothing yet.
    const loading = stub();
    draw({ compacting: true }, loading);
    expect(drawn(), 'the line draws while the first page is still coming').toContain(
      'Compacting context',
    );

    const empty = stub();
    draw({ compacting: true }, empty);
    empty.answer([]);
    expect(drawn(), 'and on a seat with no history').toContain('Compacting context');
  });

  it('draws the seat that has no session behind it as its own state', () => {
    const server = stub();
    draw({ waking: true, reason: 'no model declared' }, server);

    expect(drawn()).toContain('not running');
    expect(drawn()).toContain('no model declared');
  });

  it('draws a turn of interleaved peer messages as one group, both asks on one lane', () => {
    // **This one mounts rather than renders**, because the row is where a keyed
    // list lives: two runs of the same kind once drew two lanes with one name,
    // and a duplicate key stops the whole turn drawing at mount - which an SSR
    // render shows none of, because it writes duplicate-keyed markup happily.
    const envelope = (text: string): unknown => ({
      type: 'user',
      uuid: `u-${text.length}`,
      message: { role: 'user', content: [{ type: 'text', text }] },
    });
    const server = stub();
    draw({}, server);
    server.answer([
      {
        key: 't1',
        messages: [
          envelope(
            "[Question id=q-1 from agent 'forge/steward' (org 'Busytools') - reply with agents__tell in_reply_to=q-1]\n\nis it filed?",
          ),
          envelope("[Message id=t-2 from agent 'gateway-backend' (org 'Gateway')]\n\nFYI"),
          envelope(
            "[Question id=q-3 from agent 'forge/steward' (org 'Busytools') - reply with agents__tell in_reply_to=q-3]\n\nand the wake?",
          ),
        ],
      },
    ]);

    const html = document.body.innerHTML;
    expect(drawn(), 'every message is on the page, both asks included').toContain('3 messages');
    expect((html.match(/>ask</g) ?? []).length, 'the two asks share one lane').toBe(1);
    expect((html.match(/>message</g) ?? []).length, 'and the message its own').toBe(1);
    expect(html, 'a counterparty in this project').toContain('i-bot');
    expect(html, 'and one somewhere else').toContain('i-away');
    expect(html, 'with its org on the row').toContain('Gateway');
  });

  it('draws a tool run whose lanes share a word, which a server named after a family reaches', () => {
    // A lane's word is not an identity: `labelOf` writes a family word for a
    // built-in and an MCP SERVER's name for its tools, so a `Read` beside
    // `mcp__read__query` is two lanes both called `read`. The fold's own dedupe
    // reads a family as `(label, row kind)`, and the lane's handle is that pair
    // - keying it by the word alone is the duplicate-key crash one component
    // over from the message rows, and this mount is what reaches it.
    const server = stub();
    draw({}, server);
    server.answer([
      {
        key: 't1',
        messages: [
          {
            type: 'assistant',
            message: {
              id: 'm1',
              role: 'assistant',
              model: 'claude-opus-5',
              content: [
                { type: 'tool_use', id: 'c1', name: 'Read', input: { file_path: 'a.rs' } },
                { type: 'tool_use', id: 'c2', name: 'mcp__read__query', input: {} },
              ],
            },
          },
        ],
      },
    ]);

    const html = document.body.innerHTML;
    expect(drawn(), 'both lanes drew, so the turn drew').toContain('2 tool calls');
    expect((html.match(/>read</g) ?? []).length, 'and each kept its own word').toBe(2);
  });

  it('draws a message whose body repeats a paragraph, which a text key refuses', () => {
    // The same class as the lanes: a paragraph keyed by its own words collides
    // the moment a body says the same thing twice, and a keyed list refuses the
    // duplicate at mount.
    const server = stub();
    draw({}, server);
    server.answer([
      {
        key: 't1',
        messages: [
          {
            type: 'user',
            uuid: 'u-repeated',
            message: {
              role: 'user',
              content: [
                {
                  type: 'text',
                  text: "[Message id=t-rep from agent 'forge/steward' (org 'Busytools')]\n\nsame\n\nsame",
                },
              ],
            },
          },
        ],
      },
    ]);

    expect(drawn(), 'the message drew, both paragraphs of it').toContain('1 message');
    expect((document.body.innerHTML.match(/<p>same<\/p>/g) ?? []).length, 'both are drawn').toBe(2);
  });

  it('keeps a call the reader opened open when the turn is sent again', () => {
    // The page landing replaces the turn with the server's copy and the row is
    // updated IN PLACE - the DOM node survives - so a row that draws its open
    // state from a prop closes the moment anything lands. The state has to be
    // the element's own.
    const call = {
      type: 'assistant',
      uuid: 'a-1',
      message: {
        id: 'm1',
        role: 'assistant',
        model: 'claude-opus-5',
        content: [
          { type: 'text', text: 'reading it' },
          { type: 'tool_use', id: 'c1', name: 'Read', input: { file_path: 'a.rs' } },
        ],
      },
    };
    const answered = {
      type: 'user',
      uuid: 'u-r1',
      message: {
        role: 'user',
        content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'the file' }],
      },
    };

    const server = stub();
    draw({}, server);
    server.answer([], null);
    server.send({ kind: 'update', update: { chat_appended: { key: LEAD, msg: call } } });

    const leaf = document.querySelector('details.leaf');
    if (!(leaf instanceof HTMLDetailsElement)) throw new Error('the call did not draw');
    leaf.open = true;
    leaf.dispatchEvent(new Event('toggle'));

    // The page read lands with the same turn, grown by its result.
    server.answer([{ key: null, messages: [call, answered] }], '1');

    const after = document.querySelector('details.leaf');
    expect(after, 'the row the reader opened').toBe(leaf);
    expect((after as HTMLDetailsElement).open, 'is still open after the turn is re-sent').toBe(
      true,
    );
  });

  it('keeps a call the reader closed closed when the turn is sent again', () => {
    // The other direction, and it needs the mutation to see it: a mutation's
    // diff is drawn open, so a row the reader CLOSES used to be opened again by
    // the next update. A test that only opens things cannot tell the two
    // behaviours apart. (A run's `open` is a constant in the template, which is
    // written once at creation and never re-applied - measured, not assumed.)
    const edit = {
      type: 'assistant',
      uuid: 'a-1',
      message: {
        id: 'm1',
        role: 'assistant',
        model: 'claude-opus-5',
        content: [
          { type: 'text', text: 'fixing it' },
          {
            type: 'tool_use',
            id: 'c1',
            name: 'Edit',
            input: { file_path: 'a.rs', old_string: 'a', new_string: 'b' },
          },
        ],
      },
    };
    const answered = {
      type: 'user',
      uuid: 'u-r1',
      message: {
        role: 'user',
        content: [{ type: 'tool_result', tool_use_id: 'c1', content: 'edited' }],
      },
    };

    const server = stub();
    draw({}, server);
    server.answer([], null);
    server.send({ kind: 'update', update: { chat_appended: { key: LEAD, msg: edit } } });

    const run = document.querySelector('details.kind');
    const leaf = document.querySelector('details.leaf');
    if (!(run instanceof HTMLDetailsElement) || !(leaf instanceof HTMLDetailsElement)) {
      throw new Error('the run did not draw');
    }
    expect(leaf.open, 'a mutation draws open without being asked').toBe(true);
    for (const row of [run, leaf]) {
      row.open = false;
      row.dispatchEvent(new Event('toggle'));
    }

    // The page lands with the turn grown, which is what re-renders the row.
    server.answer([{ key: null, messages: [edit, answered] }], '1');

    expect(
      (document.querySelector('details.leaf') as HTMLDetailsElement).open,
      'the call the reader closed',
    ).toBe(false);
  });

  it('draws a peer message the socket sends live, through the frame the server forges', () => {
    // #1376: the server forges the frame a delivery needs and sends it beside
    // the typed update, so a peer message draws live through the `chat_appended`
    // the client already handles. Its own turn, because a user frame is what a
    // turn opens on - what matters here is that the row draws at all.
    const server = stub();
    draw({}, server);
    server.answer([]);
    server.send({
      kind: 'update',
      update: {
        chat_appended: {
          key: LEAD,
          msg: {
            type: 'user',
            uuid: 'u-live',
            message: {
              role: 'user',
              content: [
                {
                  type: 'text',
                  text: "[Message id=t-live from agent 'forge/steward' (org 'Busytools')]\n\npicking it up",
                },
              ],
            },
          },
        },
      },
    });

    const html = document.body.innerHTML;
    expect(drawn(), 'the message is on the page as a group of one').toContain('1 message');
    expect(html, 'marked by the counterparty class').toContain('i-bot');
    expect(html, 'and labelled by its sender').toContain('forge/steward');
  });

  it('pins the running row above the box, and out of the turn it belongs to', () => {
    // **The wiring, and both halves of it.** The row moves out of the newest
    // turn while it is being written, so a column that goes on drawing it in
    // the turn draws it twice, and a column that keeps it only in the turn
    // loses the pin. Either one alone reads as a working page.
    const server = stub();
    draw({}, server);
    server.answer([]);
    server.send(appended(frame('a-run', 100)));

    expect(document.querySelectorAll('.strip'), 'one pinned row').toHaveLength(1);
    expect(document.querySelectorAll('.strip .ring'), 'carrying the running mark').toHaveLength(1);
    expect(
      document.querySelectorAll('details.turninfo'),
      'and the turn draws no second copy of it',
    ).toHaveLength(0);
  });

  it('pins the row for a turn the seat says is running, not only one the frames built', () => {
    // A turn the client reached mid-flight has its row from a page, so `live`
    // is false there and the core's own answer is the only carrier - the state
    // a pin reading one of the two draws nothing at all for.
    const server = stub();
    draw({}, server);
    server.answer([{ key: 'turn-mid', messages: [frame('a-mid', 100)] }]);
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: true } },
    });

    expect(document.querySelectorAll('.strip'), 'the row is pinned').toHaveLength(1);
    expect(document.querySelectorAll('.strip .ring'), 'with the running mark').toHaveLength(1);
    expect(document.querySelectorAll('details.turninfo'), 'and out of the turn').toHaveLength(0);
  });

  it('moves the finished row into the pin for its beat, and into the turn when the beat ends', () => {
    // **The beat is a move, not a copy.** The turn's own row stands aside for
    // exactly as long as the pin holds it: without that the same figures draw
    // twice on one page for the 450ms the beat lasts, which is the one moment
    // it exists to serve.
    vi.useFakeTimers();
    try {
      const server = stub();
      draw({}, server);
      server.answer([]);
      server.send(appended(frame('a-run', 100)));
      expect(document.querySelectorAll('.strip'), 'the running row is pinned').toHaveLength(1);

      server.send(ended());
      server.answer([{ key: 'turn-a-run', messages: [frame('a-run', 100), settledFrame()] }]);

      expect(
        document.querySelector('.strip')?.textContent,
        'the pin holds the finished row',
      ).toContain('cumulative');
      expect(
        document.querySelectorAll('details.turninfo'),
        'and the turn draws none of it while the pin has it',
      ).toHaveLength(0);

      vi.advanceTimersByTime(450);
      flushSync();

      expect(document.querySelector('.strip'), 'the pin lets go').toBeNull();
      expect(
        document.querySelectorAll('details.turninfo'),
        'and the turn has its row',
      ).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('takes the pin back for the next turn, and beats its own finish', () => {
    // A beat left armed by the turn before shows up here: it fires mid-flight,
    // clears the turn the pin is carrying, and the next turn's own finish then
    // never beats.
    vi.useFakeTimers();
    try {
      const server = stub();
      draw({}, server);
      server.answer([]);
      server.send(appended(frame('a-one', 100)));
      server.send(ended());
      server.answer([{ key: 'turn-a-one', messages: [frame('a-one', 100), settledFrame()] }]);
      expect(
        document.querySelector('.strip')?.textContent,
        'the first finish is beating',
      ).toContain('cumulative');

      server.send(appended(frame('a-two', 700)));
      expect(
        document.querySelector('.strip')?.textContent,
        'the next turn draws its own figures',
      ).toContain('700\u{2191}');

      vi.advanceTimersByTime(450);
      flushSync();

      expect(
        document.querySelector('.strip'),
        'and stays pinned through the beat that belonged to the turn before',
      ).not.toBeNull();

      server.send(ended());
      server.answer([{ key: 'turn-a-two', messages: [frame('a-two', 700), settledFrame()] }]);

      expect(document.querySelector('.strip')?.textContent, 'and its own finish beats').toContain(
        'cumulative',
      );

      // **And its beat lets go.** The assertion above is satisfied by a row
      // that is stuck exactly as well as by one that is beating, so this is the
      // half that says the second turn's beat was armed at all: a take-back
      // that clears the timer but leaves the flag set holds this row for good,
      // and the turn below never gets its row back.
      vi.advanceTimersByTime(450);
      flushSync();

      expect(document.querySelector('.strip'), 'and its own beat lets go').toBeNull();
      expect(
        document.querySelectorAll('details.turninfo'),
        'and the turns take their rows back',
      ).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('pins nothing for a conversation whose newest turn has settled', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 'turn-done', messages: [frame('a-done', 100), settledFrame()] }]);

    expect(document.querySelectorAll('.strip'), 'no row is pinned').toHaveLength(0);
    expect(
      document.querySelectorAll('details.turninfo'),
      'and the turn draws its own',
    ).toHaveLength(1);
  });

  it('holds one of two finished rows, and leaves the other in the turn', () => {
    // **The pin holds one row, so the turn stands aside for one.** A turn can
    // carry two report rows - one per Result that landed in it, which is what a
    // refused ask leaves behind - and a column that withheld every report row
    // from the turn would lose the row the pin never took, for as long as the
    // pin holds the one it did.
    vi.useFakeTimers();
    try {
      const server = stub();
      draw({}, server);
      server.answer([]);
      server.send(appended(frame('a-run', 100)));

      server.send(ended());
      server.answer([
        {
          key: 'turn-a-run',
          messages: [frame('a-run', 100), settledFrame('r-1', 12.5), settledFrame('r-2', 9)],
        },
      ]);

      expect(document.querySelector('.strip')?.textContent, 'the pin holds the last row').toContain(
        '$9.00 cumulative',
      );
      expect(
        document.querySelectorAll('details.turninfo'),
        'and the turn keeps the row the pin never took',
      ).toHaveLength(1);
      expect(
        document.querySelector('details.turninfo')?.textContent,
        'which is the row of the first Result',
      ).toContain('$12.50 cumulative');

      vi.advanceTimersByTime(450);
      flushSync();

      expect(
        document.querySelectorAll('details.turninfo'),
        'and takes both once the pin lets go',
      ).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('pins the row of the newest turn, not of the first one the column holds', () => {
    const server = stub();
    draw({}, server);
    server.answer([
      { key: 't1', messages: [frame('a-one', 100), settledFrame()] },
      { key: 't2', messages: [frame('a-two', 200), settledFrame()] },
    ]);
    server.send(appended(frame('a-three', 700)));

    expect(document.querySelectorAll('.strip'), 'one pinned row').toHaveLength(1);
    expect(document.querySelector('.strip')?.textContent, 'carrying the newest turn').toContain(
      '700\u{2191}',
    );
    expect(
      document.querySelectorAll('details.turninfo'),
      'and the settled turns draw their own',
    ).toHaveLength(2);
  });

  it('keeps a row the reader opened mounted when a thinking row lands above it', () => {
    // A thinking unit lands ABOVE the run it interrupted, so every unit below
    // it shifts position - and a list keyed by position remounts that whole
    // subtree, closing whatever the reader had open, again for every thought on
    // a turn that keeps running. The units carry an identity of their own so
    // the row is moved rather than rebuilt, and this pins the DOM element -
    // the element itself surviving is the observable, because a remount is a
    // new element where the reader had an open one.
    const frame = (uuid: string, content: unknown[]): unknown => ({
      type: 'assistant',
      uuid,
      message: { id: `m-${uuid}`, role: 'assistant', model: 'claude-opus-5', content },
    });
    const appended = (msg: unknown): ServerMessage => ({
      kind: 'update',
      update: { chat_appended: { key: LEAD, msg } },
    });
    const server = stub();
    draw({}, server);
    server.answer([]);
    server.send(
      appended({
        type: 'user',
        uuid: 'u-live',
        message: { role: 'user', content: [{ type: 'text', text: 'go' }] },
      }),
    );
    server.send(
      appended(
        frame('a1', [
          { type: 'tool_use', id: 'toolu_r1', name: 'Read', input: { file_path: 'src/lib.rs' } },
        ]),
      ),
    );

    // The call drew as a row that opens, and the reader opens it.
    const row = document.querySelector<HTMLDetailsElement>('details.leaf');
    expect(row, 'the call drew as a row that opens').not.toBeNull();
    if (row !== null) row.open = true;

    // Now a thought lands between that call and whatever comes next.
    server.send(
      appended(frame('a2', [{ type: 'thinking', thinking: 'about the file', signature: 's' }])),
    );

    expect(row?.isConnected, 'the row the reader opened is the row still on the page').toBe(true);
    expect(row?.open, 'and it is still open').toBe(true);
    expect(drawn(), 'the thought drew beside it').toContain('about the file');
  });

  it('draws a record it is handed frozen, because the column writes into nothing it is given', () => {
    // Both halves are frozen here: the rows as they arrive off the wire, and -
    // by the module mock above - the conversation the class composes and hands
    // the column. A write into either throws in strict mode, so this failing
    // is what a write in place would look like.
    const frame = (uuid: string, text: string): unknown => ({
      type: 'assistant',
      uuid,
      message: {
        id: `m-${uuid}`,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text }],
      },
    });
    const server = stub();
    draw({}, server);

    server.answer(freeze([{ key: 't1', messages: [frame('a1', 'first words')] }]));
    server.send(appended(freeze(frame('a2', 'second words'))));

    expect(drawn(), 'the frozen page drew').toContain('first words');
    expect(drawn(), 'and the frozen frame drew with it').toContain('second words');
  });

  it('hands its readers a frozen record, which is what the guard above rests on', () => {
    // **The freeze is asserted rather than assumed.** What this pins is that
    // the `Chat` THIS FILE resolves publishes a frozen record - the import the
    // mounted column shares - so a mock that stopped matching would show up
    // here rather than silently leaving the guard above unarmed.
    const server = stub();
    const chat = new HandedChat(server.connection, LEAD);
    const seen: Conversation[] = [];
    chat.value.subscribe((value) => seen.push(value));
    chat.start();
    server.answer([
      {
        key: 't1',
        messages: [{ type: 'user', uuid: 'u-1', message: { role: 'user', content: [] } }],
      },
    ]);

    const handed = seen.at(-1);
    expect(handed, 'a conversation was published').toBeDefined();
    expect(() => handed?.turns.at(-1)?.messages.push({}), 'and it is frozen').toThrow(TypeError);
  });
});

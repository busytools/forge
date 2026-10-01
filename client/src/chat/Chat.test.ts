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
});

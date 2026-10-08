// @vitest-environment jsdom
import { createRawSnippet, flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { Chat as HandedChat, type Conversation } from './conversation';
import { echoes } from './echoes.svelte';
import { git } from './git.svelte';
import { freeze } from './testing/frozen';
import { subjectKey } from '../protocol';
import type { ClientMessage, ServerMessage, SessionUpdate } from '../protocol';
import type { Connection } from '../socket';
import { Stores } from '../stores';
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
  const dispatched: ClientMessage[] = [];
  const stores = new Stores();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'loading' }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: (message: ClientMessage) => {
      dispatched.push(message);
      return null;
    },
    more: (conversation: SessionSlot, before: string | null, turns: number) => {
      asks.push({ kind: 'more', conversation, before, turns });
      return true;
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    // The browser segment of the strip registers a role listener and reads
    // the role as it draws, so those two answer; override is never pressed
    // here and stays out.
    browserRole: () => false,
    onBrowserRole: () => () => undefined,
    takeBrowserRole: () => undefined,
    store: (what: Parameters<Connection['store']>[0]) => stores.get(what),
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;

  return {
    connection,
    asks,
    dispatched,
    send(message: ServerMessage): void {
      for (const fn of listeners) fn(message);
      flushSync();
    },
    /** The page the server would answer `more` with. */
    answer(turns: unknown[], cursor: string | null = null): void {
      this.send({ kind: 'page', conversation: LEAD, turns, cursor });
    },
    /**
     * A frame the connection holds for a seat: folded into the store, not yet
     * painted into any record this page draws - the gap a send reads across.
     */
    hold(slot: SessionSlot, ...updates: SessionUpdate[]): void {
      const store = stores.open({ session: slot });
      for (const update of updates) store.push(update);
    },
  };
}

let app: Record<string, unknown> | null = null;

function draw(props: Record<string, unknown>, server: ReturnType<typeof stub>): void {
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection, ...props },
  });
  flushSync();
}

/** What the column reads as, which is what a reader has to go on. */
const drawn = (): string => document.body.textContent ?? '';

/**
 * The painted frame a stream frame's draw lands on (#1670).
 *
 * A stream frame is folded the moment it arrives and DRAWN on the next
 * painted frame - the burst a return delivers is one draw of the latest, not
 * a replay - so a test that reads the column after one waits for that frame.
 * A page or a refusal is a state rather than a step, and is drawn at once.
 */
async function painted(): Promise<void> {
  // A test on fake timers owns the clock the frames are on, so the frames are
  // advanced rather than waited for.
  if (vi.isFakeTimers()) {
    await vi.advanceTimersByTimeAsync(34);
    flushSync();
    return;
  }
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  flushSync();
}

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
  git.sync(null);
});

describe('the chat column as it draws', () => {
  /**
   * **The queue sits above the pin, and both above the box.** The queue's
   * DATA is the page's, but its place is the column's: what is waiting reads
   * against what is running, and the strip keeps its place right above the
   * composer.
   */
  it('draws the queue snippet above the strip', () => {
    const server = stub();
    const queue = createRawSnippet(() => ({ render: () => '<span class="pile"></span>' }));
    git.sync({
      label: 'feat/x',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    draw({ queue }, server);
    // A page with a turn on it: the column's empty state owns a seat with no
    // history, and the strip and the queue live in the drawn column.
    server.answer([{ key: 't1', messages: [] }]);

    const pile = document.querySelector('.pile');
    const strip = document.querySelector('.strip');
    expect(pile, 'the queue snippet did not draw').not.toBeNull();
    expect(strip, 'the strip did not draw').not.toBeNull();
    if (pile === null || strip === null) return;
    expect(
      pile.compareDocumentPosition(strip) & Node.DOCUMENT_POSITION_FOLLOWING,
      'the strip did not follow the queue in the column',
    ).not.toBe(0);
  });

  /**
   * **The strip stands in every conversation state, not only under the
   * list.** A seat coming up, refused, not yet read or empty holds the same
   * tree, tasks and watchers as one mid-turn - and the rows are the whole
   * way into them, so nesting the strip back inside any one state's arm is
   * the regression this pins.
   */
  it('draws the strip on seats with no list at all', async () => {
    const server = stub();
    git.sync({
      label: 'feat/x',
      head: "the project's tree",
      ahead: null,
      uncommitted: null,
      pr: null,
      gate: null,
    });
    const redraw = async (props: Record<string, unknown>): Promise<void> => {
      if (app !== null) await unmount(app);
      document.body.innerHTML = '';
      draw(props, server);
    };

    await redraw({ waking: true });
    expect(document.querySelector('.strip'), 'no strip on a seat coming up').not.toBeNull();

    await redraw({});
    expect(document.querySelector('.strip'), 'no strip before the first read').not.toBeNull();

    server.send({ kind: 'error', what: 'more', why: 'the conversation is not held yet' });
    expect(
      document.querySelector('.strip'),
      'no strip on a refused seat with nothing under it',
    ).not.toBeNull();

    await redraw({});
    server.answer([]);
    expect(document.querySelector('.strip'), 'no strip on an empty seat').not.toBeNull();
  });

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

  it('keeps a row the reader opened when a page prefixes the turn with their words', async () => {
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
    await painted();

    const row = document.querySelector<HTMLDetailsElement>('details.leaf');
    expect(row, 'the call drew as a row that opens').not.toBeNull();
    if (row !== null) row.open = true;

    server.answer([{ key: null, messages: [said('go'), call] }]);
    flushSync();

    expect(row?.isConnected, 'the row the reader opened is the row still on the page').toBe(true);
    expect(row?.open, 'and it is still open').toBe(true);
    expect(drawn(), 'and the reader own words drew above it').toContain('go');
  });

  it('wakes a lead rather than drawing it as one nothing runs', () => {
    // A lead's page dispatches the spawn on open, so this state is a wait
    // with a keeper: the waking line sweeps rather than refusing.
    const server = stub();
    draw({ waking: true, reason: 'no model declared' }, server);

    expect(drawn()).toContain('Waking up agent...');
    expect(document.querySelector('.hold .shimmer'), 'the wake does not sweep').not.toBeNull();
    expect(drawn(), 'the wake line read as a refusal').not.toContain('not running');
  });

  it('keeps the waking line up while the spawn is in flight', () => {
    // The wake reads as one state on both sides of the roster noticing:
    // before the row lands (waking) and while the core brings it up
    // (spawning), so the column never flashes a not-running line in between.
    const server = stub();
    draw({ spawning: true }, server);

    expect(drawn(), 'the spawn in flight lost its waking line').toContain('Waking up agent...');
    expect(
      document.querySelector('.hold .shimmer'),
      'the spawn line does not sweep',
    ).not.toBeNull();
  });

  it('keeps the waking line up for a worker seat the core is spawning', () => {
    // The roster can name a WORKER Spawning (its lead's spawn), and the
    // not-running refusal there would read as a dead seat at the moment it is
    // coming up - the inner arm has to answer for any spawning seat, not
    // only a lead.
    const server = stub();
    draw({ spawning: true, slot: { ...LEAD, label: 'w1' } }, server);

    expect(drawn(), 'a spawning worker read as a dead seat').toContain('Waking up agent...');
    expect(drawn(), 'a spawning worker drew the refusal').not.toContain('not running');
  });

  it('draws a seat that has no session behind it as its own state', () => {
    const server = stub();
    draw({ waking: true, reason: 'no model declared', slot: { ...LEAD, label: 'w1' } }, server);

    expect(drawn()).toContain('not running');
    expect(drawn()).toContain('no model declared');
  });

  it('draws a turn of interleaved peer messages as rows of one turn, both asks included', () => {
    // **This one mounts rather than renders**, because the row is where a keyed
    // list lives: duplicate keys stop the whole turn drawing at mount, which an
    // SSR render shows none of.
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
          envelope("[Message id=m-1 from agent 'forge/steward' (org 'Busytools')]\n\nis it filed?"),
          envelope("[Message id=m-2 from agent 'gateway-backend' (org 'Gateway')]\n\nFYI"),
          envelope(
            "[Message id=m-3 from agent 'forge/steward' (org 'Busytools')]\n\nand the wake?",
          ),
        ],
      },
    ]);

    const html = document.body.innerHTML;
    expect(drawn(), 'every message is on the page, both asks included').toContain('is it filed?');
    expect(html, 'the rows say which way each went').toContain('>from</span>');
    expect(html, 'carrying the incoming mark').toContain('i-inbox');
    expect(html, 'with the org of a counterparty outside the reader own').toContain('Gateway');
  });

  it('draws a built-in call beside a server tool named after a family', () => {
    // `mcp__read__query` beside a `Read` was once two lanes both called
    // `read`, and a word-keyed handle is a duplicate-key crash at mount - so
    // this mount is what catches a regression to keying rows by anything
    // shared. The rows now carry their own kinds: the family's glyph and the
    // mcp one.
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
    expect(drawn(), 'both calls drew, so the turn drew').toContain('a.rs');
    expect(html, 'the read row carries its own glyph').toContain('i-read');
    expect(html, 'and the server tool the mcp glyph').toContain('i-mcp');
  });

  it('draws a message whose body repeats a paragraph, which a text key refuses', () => {
    // The same class as the rows' keys: a paragraph keyed by its own words
    // collides the moment a body says the same thing twice, and a keyed list
    // refuses the duplicate at mount.
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

    expect(drawn(), 'the message drew').toContain('forge/steward');
    expect((document.body.innerHTML.match(/<p>same<\/p>/g) ?? []).length, 'both are drawn').toBe(2);
  });

  it('keeps a call the reader opened open when the turn is sent again', async () => {
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
    await painted();

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

  it('keeps a call the reader closed closed when the turn is sent again', async () => {
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
    await painted();

    const leaf = document.querySelector('details.leaf');
    if (!(leaf instanceof HTMLDetailsElement)) throw new Error('the call did not draw');
    expect(leaf.open, 'a mutation draws open without being asked').toBe(true);
    leaf.open = false;
    leaf.dispatchEvent(new Event('toggle'));

    // The page lands with the turn grown, which is what re-renders the row.
    server.answer([{ key: null, messages: [edit, answered] }], '1');

    expect(
      (document.querySelector('details.leaf') as HTMLDetailsElement).open,
      'the call the reader closed',
    ).toBe(false);
  });

  it('draws a peer message the socket sends live, through the frame the server forges', async () => {
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

    await painted();

    const html = document.body.innerHTML;
    expect(drawn(), 'the message is on the page').toContain('picking it up');
    expect(html, 'marked by the incoming direction').toContain('i-inbox');
    expect(html, 'and labelled by its sender').toContain('forge/steward');
  });

  it('pins the running row above the box, and out of the turn it belongs to', async () => {
    // **The wiring, and both halves of it.** The row moves out of the newest
    // turn while it is being written, so a column that goes on drawing it in
    // the turn draws it twice, and a column that keeps it only in the turn
    // loses the pin. Either one alone reads as a working page.
    const server = stub();
    draw({}, server);
    server.answer([]);
    server.send(appended(frame('a-run', 100)));
    await painted();

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

  it('moves the finished row into the pin for its beat, and into the turn when the beat ends', async () => {
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
      await painted();
      expect(document.querySelectorAll('.strip'), 'the running row is pinned').toHaveLength(1);

      server.send(ended());
      server.answer([{ key: 'turn-a-run', messages: [frame('a-run', 100), settledFrame()] }]);
      await painted();

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

  it('takes the pin back for the next turn, and beats its own finish', async () => {
    // A beat left armed by the turn before shows up here: it fires mid-flight,
    // clears the turn the pin is carrying, and the next turn's own finish then
    // never beats.
    vi.useFakeTimers();
    try {
      const server = stub();
      draw({}, server);
      server.answer([]);
      server.send(appended(frame('a-one', 100)));
      await painted();
      server.send(ended());
      server.answer([{ key: 'turn-a-one', messages: [frame('a-one', 100), settledFrame()] }]);
      await painted();
      expect(
        document.querySelector('.strip')?.textContent,
        'the first finish is beating',
      ).toContain('cumulative');

      server.send(appended(frame('a-two', 700)));
      await painted();
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
      await painted();

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

  it('holds one of two finished rows, and leaves the other in the turn', async () => {
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
      await painted();

      server.send(ended());
      server.answer([
        {
          key: 'turn-a-run',
          messages: [frame('a-run', 100), settledFrame('r-1', 12.5), settledFrame('r-2', 9)],
        },
      ]);
      await painted();

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

  it('pins the row of the newest turn, not of the first one the column holds', async () => {
    const server = stub();
    draw({}, server);
    server.answer([
      { key: 't1', messages: [frame('a-one', 100), settledFrame()] },
      { key: 't2', messages: [frame('a-two', 200), settledFrame()] },
    ]);
    server.send(appended(frame('a-three', 700)));
    await painted();

    expect(document.querySelectorAll('.strip'), 'one pinned row').toHaveLength(1);
    expect(document.querySelector('.strip')?.textContent, 'carrying the newest turn').toContain(
      '700\u{2191}',
    );
    expect(
      document.querySelectorAll('details.turninfo'),
      'and the settled turns draw their own',
    ).toHaveLength(2);
  });

  it('keeps a row the reader opened mounted when a thinking row lands above it', async () => {
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
    await painted();

    // The call drew as a row that opens, and the reader opens it.
    const row = document.querySelector<HTMLDetailsElement>('details.leaf');
    expect(row, 'the call drew as a row that opens').not.toBeNull();
    if (row !== null) row.open = true;

    // Now a thought lands between that call and whatever comes next.
    server.send(
      appended(frame('a2', [{ type: 'thinking', thinking: 'about the file', signature: 's' }])),
    );
    await painted();

    expect(row?.isConnected, 'the row the reader opened is the row still on the page').toBe(true);
    expect(row?.open, 'and it is still open').toBe(true);
    expect(drawn(), 'the thought drew beside it').toContain('about the file');
  });

  it('draws a record it is handed frozen, because the column writes into nothing it is given', async () => {
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
    await painted();

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

describe('the reader own words before the core has them', () => {
  /** The seat's key, which is what a pending send is held under. */
  const key = subjectKey({ session: LEAD });

  /** A turn's own opening words, which is the frame the core echoes a prompt in. */
  const said = (text: string): unknown => ({
    type: 'user',
    uuid: `u-${text}`,
    message: { role: 'user', content: [{ type: 'text', text }] },
  });

  afterEach(() => {
    echoes.clear(key);
  });

  it('draws the words while they are on their way, and stops when the core has them', async () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    echoes.post(key, 'and run the gate too', false, 'e-gate');
    flushSync();
    expect(drawn(), 'the words are drawn before the core has them').toContain(
      'and run the gate too',
    );
    expect(drawn(), 'and the row says so').toContain('sending');

    // The core starting the turn takes the MARK off, not the words: the core's
    // own copy of a prompt forge injected is not echoed as a frame, so the row
    // is the reader's message until the next page read carries one.
    echoes.take(key);
    flushSync();
    expect(drawn(), 'the words stay on screen').toContain('and run the gate too');
    expect(
      drawn(),
      'and the mark holds its beat, which is what makes an instant answer visible',
    ).toContain('sending');

    // The core's own copy is the signal, and it arrives as its own frame.
    server.send(appended(said('and run the gate too')));
    await painted();
    expect(echoes.of(key), 'the core having the words is what settles the row').toBeUndefined();
  });

  it('leaves a mid-turn send to the pile, which is the only thing drawing it', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    // Posted with the seat already running: the CLI queues it, and the row's
    // home is the pile above the box. Drawing it here too would say "sent" in
    // the chat and "queued" in the pile about one prompt.
    echoes.post(key, 'queued while the gate runs', true, 'e-q1');
    flushSync();
    expect(drawn(), 'the chat does not claim a prompt the pile is holding').not.toContain(
      'queued while the gate runs',
    );
    expect(echoes.of(key)?.state, 'the send is still held, so a refusal can still reach it').toBe(
      'sending',
    );
  });

  it('draws a mid-turn send that was REFUSED, because the pile never saw it', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    // Posted into a running seat, so the pile would have drawn it - but the
    // dispatch was refused, so no queued row exists anywhere. The suppression
    // is for the WAITING state alone: a failure has to be visible, or the
    // words are lost behind a row that never comes.
    echoes.post(key, 'queued but refused', true, 'e-q2');
    echoes.refuse(key, 'the socket closed mid-send');
    flushSync();

    expect(drawn(), 'a refused send draws wherever it was sent from').toContain(
      'queued but refused',
    );
    expect(drawn(), 'and the row says why').toContain('not sent · the socket closed mid-send');
  });

  it('stops when a page read carries the words, which is where a dropped send lands', () => {
    // The other end of a turn: a read rebuilds the turn with the reader's own
    // words at its head, which is the shape a send that outlived a dropped
    // socket comes back in. Read only from the ends, so this end has to be
    // looked at as well as the appended one.
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    echoes.post(key, 'and the gate again', false, 'e-again');
    flushSync();
    expect(drawn(), 'the row is up before the read lands').toContain('and the gate again');

    server.answer([{ key: 't2', messages: [said('and the gate again'), frame('a2', 12)] }]);
    expect(echoes.of(key), 'the page carrying the words at the head settles it').toBeUndefined();
  });

  it('draws a send on a seat with no history, which is the first thing it says', () => {
    const server = stub();
    draw({}, server);
    server.answer([]);
    expect(drawn(), 'nothing is claimed about a seat that has said nothing').toContain(
      'Nothing said yet',
    );

    echoes.post(key, 'start here', false, 'e-start');
    flushSync();
    expect(drawn(), 'the first thing the seat says is the reader own words').toContain(
      'start here',
    );
    expect(drawn(), 'and the empty copy goes with them').not.toContain('Nothing said yet');
  });

  it('keeps a refused send, names the reason, and sends it again from the row', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    echoes.post(key, 'and run the gate too', false, 'e-gate');
    echoes.refuse(key, 'the session is not running');
    flushSync();
    expect(drawn(), 'the words stay where they were sent from').toContain('and run the gate too');
    expect(drawn(), 'and the row says why they did not go').toContain(
      'not sent · the session is not running',
    );

    const retry = document.querySelector<HTMLButtonElement>('.mine .retry');
    if (retry === null) throw new Error('the failed row drew no way to send it again');
    retry.click();
    flushSync();

    const resentCommand = server.dispatched.at(-1);
    expect(resentCommand, 'the row sends the same words again').toMatchObject({
      prompt_under: {
        key: LEAD,
        text: 'and run the gate too',
        attachments: [],
        source: 'you',
      },
    });
    const resentUuid = (resentCommand as { prompt_under?: { uuid?: unknown } } | undefined)
      ?.prompt_under?.uuid;
    expect(typeof resentUuid, 'under a fresh id, which is what the pile settles by').toBe('string');
    expect(resentUuid, 'not the id the first attempt went under').not.toBe('e-gate');
    const resent = echoes.of(key);
    expect(resent?.state, 'and the row is back to saying it is on its way').toBe('sending');
    // **The mark carries the DISPATCHED id**, not merely some id: the pile
    // settles the mark by that id, so a second mint beside it would leave the
    // cancel unable to reach the retry.
    expect(resent?.id, 'and the mark names the id the retry went out under').toBe(resentUuid);
  });

  /**
   * A retry refused at the socket gets its one telling from the echo row -
   * the reason plus the way back - so the retry hands dispatch no seat and no
   * notice line joins it (the round-1 find): two phrasings of one loss read
   * as two losses.
   */
  it('tells a refused retry once, through the echo row', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    echoes.post(key, 'and run the gate too', false, 'e-gate');
    echoes.refuse(key, 'the session is not running');
    flushSync();

    // The socket closes under the retry: dispatch throws like the real one,
    // and what it was called with is what says whether a notice was asked for.
    let handed: unknown[] = [];
    server.connection.dispatch = (...args: unknown[]) => {
      handed = args;
      throw new Error('the socket is not open');
    };
    const retry = document.querySelector<HTMLButtonElement>('.mine .retry');
    if (retry === null) throw new Error('the failed row drew no way to send it again');
    retry.click();
    flushSync();

    expect(drawn(), 'the row says why').toContain('not sent · the socket is closed');
    expect(handed, 'the command alone, no seat handed over').toHaveLength(1);
  });

  /**
   * The retry reads the seat, not the turn this page happens to be drawing.
   *
   * The record is written once per painted frame, so a turn-start frame can be
   * applied and not yet drawn - and a retry posted as not-running is taken by
   * the very paint that carries the turn, which is past the point a refusal can
   * reach it.
   */
  it('reads a retry as running from the seat, not from the drawn turn', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);

    echoes.post(key, 'and run the gate too', false, 'e-gate');
    echoes.refuse(key, 'the session is not running');
    flushSync();

    // The turn is on the seat already, while the turn this page draws has not
    // been written with it.
    server.hold(LEAD, {
      chat_appended: { key: LEAD, msg: { type: 'system', subtype: 'init', session_id: 's' } },
    });

    const retry = document.querySelector<HTMLButtonElement>('.mine .retry');
    if (retry === null) throw new Error('the failed row drew no way to send it again');
    retry.click();
    flushSync();

    const mark = echoes.of(key);
    expect(
      mark?.state === 'sending' ? mark.running : null,
      'the retry was posted into the turn the seat already had',
    ).toBe(true);
  });

  /**
   * The drawn turn is the fallback where the seat has no store.
   *
   * A page that reached a seat mid-flight has its turn from the snapshot, and a
   * connection with no store of its own is what the fallback argument is for:
   * the retry must still be posted as running there, or the paint carrying the
   * turn takes it past the point a refusal can reach it.
   */
  it('reads the drawn turn when the seat has no store to read', () => {
    const server = stub();
    draw({}, server);
    server.answer([{ key: 't1', messages: [frame('a1', 12)] }]);
    server.send({
      kind: 'snapshot',
      subject: { session: LEAD },
      data: { header: { turn_in_flight: true } },
    });

    echoes.post(key, 'and run the gate too', false, 'e-gate');
    echoes.refuse(key, 'the session is not running');
    flushSync();

    const retry = document.querySelector<HTMLButtonElement>('.mine .retry');
    if (retry === null) throw new Error('the failed row drew no way to send it again');
    retry.click();
    flushSync();

    const mark = echoes.of(key);
    expect(
      mark?.state === 'sending' ? mark.running : null,
      'the retry reads the drawn turn where the connection has no store',
    ).toBe(true);
  });
});

describe('the arrival mark', () => {
  /** One page turn, whose text is all a row needs to draw. */
  const turn = (key: string): unknown => ({
    key,
    messages: [
      {
        type: 'assistant',
        message: {
          id: `m-${key}`,
          role: 'assistant',
          model: 'claude-opus-5',
          content: [{ type: 'text', text: `said ${key}` }],
        },
      },
    ],
  });

  /**
   * **The mark rides the item through the list's own `itemProps`.** Keyed on
   * the turn's tail instead, the rule marks every mounted turn at once - and a
   * stub that drops `itemProps` sees neither mistake.
   */
  it('marks the arriving item through the list, and only it', async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw({}, server);
      server.answer([turn('t1'), turn('t2')]);
      await painted();

      const items = [...document.querySelectorAll('.conv > *')];
      expect(items.length, 'the list drew no items').toBe(2);
      expect(
        items[1]?.classList.contains('arriving'),
        "the arriving item's wrapper lost its mark",
      ).toBe(true);
      expect(
        items[0]?.classList.contains('arriving'),
        'a settled item carried the arrival mark',
      ).toBe(false);

      vi.advanceTimersByTime(300);
      flushSync();

      expect(
        document.querySelector('.conv .arriving'),
        'the mark outlived its window, and would replay on a remount',
      ).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

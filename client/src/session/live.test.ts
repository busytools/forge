// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { createRawSnippet, flushSync, mount, unmount, type ComponentProps } from 'svelte';
import type { AddressInfo, RawData, WebSocketServer as Server } from 'ws';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * `ws` by its CommonJS entry, because this file runs in jsdom and the
 * package's `browser` field resolves the import to a client-only shim with no
 * server in it. The page needs a DOM to mount into and a real socket to talk
 * to, and this is the one line that gets both.
 */
const { WebSocketServer, WebSocket: ClientSocket } = createRequire(import.meta.url)('ws') as {
  WebSocketServer: typeof Server;
  WebSocket: unknown;
};

/**
 * The page's client socket, from the same place.
 *
 * Node's own `WebSocket` and jsdom's `Event` are two different `Event` classes,
 * and the one undici builds its frames with is not the one a jsdom document
 * dispatches, so the connect throws inside Node before the page sees anything.
 * `ws`'s client takes the global's place for this file.
 */
(globalThis as { WebSocket?: unknown }).WebSocket = ClientSocket;

import { get, writable } from 'svelte/store';

import { cronNames } from '../chat/cron-names.svelte';
import { git } from '../chat/git.svelte';
import { installResizeObserver } from '../chat/testing/viewport';
import { homeWire } from '../dev/fixture.data';
import sessionFixture from '../dev/fixtures/session.json';
import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import { PROTOCOL_VERSION, subjectKey } from '../protocol';
import type { Route } from '../routes';
import Router from '../shell/Router.svelte';
import { connect, type Connection, type ConnectionStatus } from '../socket';
import type { Store, StoreState, StoreValue } from '../stores';
import { DEFAULT_SETTINGS, type SessionSlot } from '../wire/types';
import { REPLACES } from './apply';
import { watchSession, type SessionRead } from './live';
import Session from './Session.svelte';
import Routes from './testing/Routes.svelte';

// This is the one page mounted over a REAL connection, so it drives the
// column's store and status branches too - and its record is frozen where the
// class publishes it, like every other file that mounts the column.
vi.mock('../chat/conversation', async (importOriginal) => {
  const { frozenConversation } = await import('../chat/testing/frozen');
  return frozenConversation(await importOriginal<typeof import('../chat/conversation')>());
});

// **The list is a stub here too, so the rows draw.** jsdom performs no layout,
// and `virtua` draws nothing until its viewport has been measured - a switch
// asserted on the drawn conversation would read an empty list either way.
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('../chat/testing/List.svelte');
  return { VList: List };
});

/**
 * The page over a REAL socket, which is the only arrangement the unit tests
 * cannot reach.
 *
 * **`render` from `svelte/server` runs no effects, so every case in
 * `Session.test.ts` draws a page with NO record** - and the record is where
 * eight of the nine sections, the header's four facts and the account chip come
 * from. This is the test that sees the populated page, and the two defects that
 * reached the browser first were both in that gap: a subject compared by
 * identity rather than by key, and a Slack target narrowed against an object
 * the socket never sends.
 */
// The column's list is `virtua`'s, and a Router-mounted page draws it: jsdom
// has no ResizeObserver to offer it.
installResizeObserver();

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A forge that answers the two subjects a session page watches. */
async function stubServer(
  session: unknown,
  conversations: Record<string, unknown[]> = {},
  home: typeof homeWire = homeWire,
) {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;

  const received: Subscribe[] = [];
  const gone: Subscribe[] = [];
  const sockets = new Set<import('ws').WebSocket>();
  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    const greeting: ServerMessage = {
      kind: 'greeting',
      version: PROTOCOL_VERSION,
      settings: DEFAULT_SETTINGS,
    };
    socket.send(JSON.stringify(greeting));
    socket.on('message', (data) => {
      const message = JSON.parse(text(data)) as Subscribe;
      if (message.kind === 'unsubscribe') {
        gone.push(message);
        return;
      }
      // The column pages for its history on the way up, so a stub that never
      // answers holds it in its loading state and nothing under the header
      // draws - the strip and the queue included.
      if (message.kind === 'more') {
        const asked = (message as { conversation?: SessionSlot }).conversation ?? LEAD;
        const key = subjectKey({ session: asked });
        const turns =
          conversations[key] ??
          (session as { conversation: { turns: unknown[] } }).conversation.turns;
        socket.send(
          JSON.stringify({
            kind: 'page',
            conversation: asked,
            turns,
            cursor: null,
          } satisfies ServerMessage),
        );
        return;
      }
      if (message.kind !== 'subscribe') return;
      received.push(message);
      const snapshot: ServerMessage = {
        kind: 'snapshot',
        subject: message.what as ServerMessage extends { subject: infer S } ? S : never,
        data: message.what === 'home' ? home : session,
      };
      socket.send(JSON.stringify(snapshot));
    });
  });

  return {
    url: `ws://127.0.0.1:${port}/socket`,
    /** Every subscribe the client made, which is where the answering role is declared. */
    received,
    /** Every unsubscribe the client sent: the other half of the count. */
    gone,
    /**
     * Drop every socket the client holds, as a laptop left open overnight
     * does: the client reconnects on its own, and nothing about it is asked.
     */
    drop() {
      for (const socket of sockets) socket.terminate();
    },
    async close() {
      for (const socket of sockets) socket.terminate();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/** One subscribe as the client sent it. */
interface Subscribe {
  kind: string;
  what?: unknown;
  answering?: boolean;
}

function text(data: RawData): string {
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
  return data.toString('utf8');
}

/** The page as a reader reads it. */
const drawn = (): string => document.body.innerHTML;

/** Let a snapshot land: a launch is a promise chain over a socket. */
async function settle(): Promise<void> {
  for (let i = 0; i < 60; i += 1) {
    flushSync();
    await new Promise((resolve) => setTimeout(resolve, 5));
    flushSync();
    if (document.querySelector('[data-k^="sec-"]') !== null) return;
  }
}

/**
 * Let a predicate hold, over a bounded number of paints.
 *
 * **A `history.back()` is an asynchronous browser event**, and one tick is a
 * race the CI machine schedules differently from a dev machine - the round
 * where the palette's entry hand-off landed passed locally on one tick and
 * failed there. The condition is what is waited on, never the tick.
 */
async function until(ok: () => boolean): Promise<void> {
  for (let i = 0; i < 60; i += 1) {
    flushSync();
    if (ok()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

let app: Record<string, unknown> | null = null;
let server: Awaited<ReturnType<typeof stubServer>> | null = null;
let connection: Connection | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  connection?.close();
  connection = null;
  await server?.close();
  server = null;
  document.body.innerHTML = '';
});

/** Mount the page against a socket answering with `session`. */
async function open(
  session: unknown,
  wire: typeof homeWire = homeWire,
  props: Record<string, unknown> = {},
): Promise<void> {
  server = await stubServer(session);
  connection = connect(server.url);
  app = mount(Session, {
    target: document.body,
    props: { slot: LEAD, connection, wire, ...props },
  });
  await settle();
}

/** What the client asked the server for, of the seat's own subject. */
function seatSubscribe(): Subscribe | undefined {
  return server?.received.find(
    (message) => message.kind === 'subscribe' && typeof message.what === 'object',
  );
}

/** How many times the client subscribed the seat, which is the count a journey is read by. */
function seatSubscribes(): number {
  return (
    server?.received.filter(
      (message) => message.kind === 'subscribe' && typeof message.what === 'object',
    ).length ?? 0
  );
}

/** One assistant frame saying `text`, which a page carries as a turn's words. */
function say(text: string): unknown {
  return {
    type: 'assistant',
    message: {
      id: `m-${text}`,
      role: 'assistant',
      model: 'claude-opus-5',
      content: [{ type: 'text', text }],
    },
  };
}

/** How many times the client subscribed ONE seat, by its key. */
function seatSubscribesOf(slot: SessionSlot): number {
  const want = subjectKey({ session: slot });
  return (
    server?.received.filter(
      (message) =>
        message.kind === 'subscribe' &&
        typeof message.what === 'object' &&
        subjectKey(message.what as Subject) === want,
    ).length ?? 0
  );
}

/** How many times the client gave ONE seat back. */
function seatUnsubscribesOf(slot: SessionSlot): number {
  const want = subjectKey({ session: slot });
  return (
    server?.gone.filter(
      (message) => typeof message.what === 'object' && subjectKey(message.what as Subject) === want,
    ).length ?? 0
  );
}

/** Leave the page, keeping the connection it was reading through. */
async function leave(): Promise<void> {
  if (app === null) throw new Error('nothing is mounted to leave');
  await unmount(app);
  app = null;
}

/** Come back to the seat over the connection the last visit left. */
function revisit(extra: Record<string, unknown> = {}): void {
  if (connection === null) throw new Error('a revisit needs the connection its last visit made');
  app = mount(Session, {
    target: document.body,
    props: { slot: LEAD, connection, wire: homeWire, ...extra },
  });
}

/**
 * The props a mounted shell needs, for the tests that go through the Router.
 *
 * The Router is the mount the app makes: it hands the session page its
 * conversation and composer columns, which is the arrangement `Session`
 * mounted on its own does not reach.
 */
function routerProps(
  home: typeof homeWire = homeWire,
): Omit<ComponentProps<typeof Router>, 'route'> {
  if (connection === null) throw new Error('the Router needs the connection the case made');
  return {
    settings: DEFAULT_SETTINGS,
    address: server?.url ?? '',
    home: { wire: home, refused: null, report: null },
    failure: null,
    connected: true,
    connection,
    notice: null,
    onconnect: () => undefined,
  };
}

describe('the session page over a socket', () => {
  it('draws the seat record the server answered with', async () => {
    await open(sessionFixture);

    // The tree row is the page's own reader of the record, so its store
    // holding the fixture's branch is the record reaching the page.
    expect(git.strip()?.label, 'the seat record never reached the page').toContain('worktree-pr');
    const facts = document.querySelector('.sess .facts');
    expect(facts?.textContent, 'the header drew no facts from the record').toContain('max');
  });

  /**
   * The tab's name is the shell's own effect, so only a mounted shell shows
   * it: the server-rendered Router suite never runs effects. A lead's seat
   * names its project, a worker's adds the worker's label.
   */
  it('names the tab after the seat the route holds', async () => {
    server = await stubServer(sessionFixture);
    connection = connect(server.url);
    const props = {
      settings: DEFAULT_SETTINGS,
      address: server.url,
      home: { wire: homeWire, refused: null, report: null },
      failure: null,
      connected: true,
      connection,
      notice: null,
      onconnect: () => undefined,
    };
    app = mount(Router, {
      target: document.body,
      props: { ...props, route: { name: 'session', slot: LEAD } },
    });
    await settle();
    expect(document.title, "a lead's tab is its project").toBe('proj');

    await unmount(app);
    document.body.innerHTML = '';
    app = mount(Router, {
      target: document.body,
      props: { ...props, route: { name: 'session', slot: { ...LEAD, label: 'w1' } } },
    });
    await settle();
    expect(document.title, 'a worker adds its own label').toBe('proj \u{b7} w1');
  });

  /**
   * **The count is the conversation's, so the header reads it off
   * `conversation` and not off `header`** - and the terminal draws it on the
   * row that carries the context figure, which is this one. A page that stops
   * at the context figure leaves a reader no way to see a session has been
   * compacted at all.
   */
  it('draws the compactions the conversation has taken, beside the context', async () => {
    await open({
      ...sessionFixture,
      conversation: { ...sessionFixture.conversation, compaction_count: 54 },
    });

    const facts = document.querySelector('.sess .facts');
    expect(facts?.textContent, 'the header drew no compaction count from the record').toContain(
      '54 compactions',
    );
  });

  /** A session with no boundary in its transcript draws no figure, not a zero. */
  it('draws no compaction figure for a session that has never compacted', async () => {
    await open(sessionFixture);

    const facts = document.querySelector('.sess .facts');
    expect(
      facts?.textContent ?? '',
      'a session that never compacted claimed a count',
    ).not.toContain('compaction');
  });

  /**
   * The rail comes from the home subject, which the shell holds for its whole
   * life - so this is also the assertion that the page reads two subjects and
   * not one.
   */
  it('draws the rail from the home the connection also carries', async () => {
    await open(sessionFixture);
    expect(drawn()).toContain('class="pr on"');
    expect(drawn()).toContain('1 live / 1');
  });

  /**
   * The mount the app actually makes, asserted rather than walked by hand: the
   * Router's session branch is what puts the page on screen, and nothing else
   * in the suite renders it.
   */
  it('reaches the page through the Router a session URL lands on', async () => {
    // A queued prompt rides the record, and this is the one arrangement where
    // the pile has no home of its own: the page handing its column the queue
    // snippet is the whole of what draws it, so dropping that prop would
    // silently stop drawing the pile everywhere.
    server = await stubServer({
      ...sessionFixture,
      state: {
        ...sessionFixture.state,
        queue: [{ uuid: 'q1', source: 'you', text: 'a queued prompt' }],
      },
    });
    connection = connect(server.url);
    app = mount(Router, {
      target: document.body,
      props: {
        route: { name: 'session', slot: LEAD },
        settings: DEFAULT_SETTINGS,
        address: '',
        home: { wire: homeWire, refused: null, report: null },
        failure: null,
        connected: true,
        connection,
        notice: null,
        onconnect: () => {},
      },
    });
    await settle();

    expect(git.strip()?.label, 'the URL-reached page drew no seat record').toContain('worktree-pr');
    expect(document.querySelector('.sess .nm')?.textContent, 'the page drew its seat').toBe('proj');
    expect(
      document.querySelector('.pile .qcard .w')?.textContent,
      'the queued row did not reach the column through the page',
    ).toBe('a queued prompt');
  });

  /**
   * **A cold switch subscribes the seat once**, and the second subscription
   * was a whole record sent twice: the page's record and the conversation
   * both read the seat's frames, and each opened a subscription of its own -
   * 6.5 MB encoded and sent twice on the dotfiles/lead seat (measured
   * 2026-10-09). The conversation's subscription is the one that outlives
   * the page (Ved, 2026-10-09), so the page rides it.
   */
  it('subscribes the seat once when the conversation mounts with the page', async () => {
    server = await stubServer(sessionFixture);
    connection = connect(server.url);
    app = mount(Router, {
      target: document.body,
      props: { ...routerProps(), route: { name: 'session', slot: LEAD } },
    });
    await settle();

    // The column is mounted and folded a page, so the count below is one seat
    // held by two readers rather than a page whose conversation never started.
    expect(
      document.querySelectorAll('.conv .turn').length,
      'precondition: the conversation column never mounted',
    ).toBeGreaterThan(0);
    expect(seatSubscribes(), 'the seat was subscribed twice for one cold switch').toBe(1);
    // **And the one subscribe is the page's own role.** The conversation's
    // hold keeps a subscription alive, it does not open one - so the page
    // that can answer is what opens it, as itself. A hold that opened it
    // first would subscribe the seat as an observer, and a prompt the page
    // could have drawn would be cancelled at birth instead.
    expect(
      seatSubscribe()?.answering,
      'the seat was subscribed under a weaker role than the page declares',
    ).toBe(true);
  });

  /**
   * **A return sends nothing while the conversation still holds the seat.**
   * The page's subscription used to be the only one, so leaving gave it back
   * and the return asked again - a whole record re-encoded and re-sent for a
   * client that still holds it. The conversation's subscription outlives the
   * page (Ved, 2026-10-09), so the seat is never given up in the first place.
   */
  it('sends nothing when the route returns to a seat the conversation holds', async () => {
    const W1: SessionSlot = { ...LEAD, label: 'w1' };
    server = await stubServer(sessionFixture);
    connection = connect(server.url);
    const route = writable<Route>({ name: 'session', slot: LEAD });
    app = mount(Routes, {
      target: document.body,
      props: { route, props: routerProps() },
    });
    await settle();
    expect(seatSubscribes(), 'precondition: the first visit subscribed the seat').toBe(1);

    // The switch subscribes the seat it moves to, and the frame has a socket
    // hop to make - so the count is waited on, never read once.
    route.set({ name: 'session', slot: W1 });
    await until(() => seatSubscribesOf(W1) === 1);
    expect(seatSubscribesOf(W1), 'precondition: the switch subscribed w1').toBe(1);

    route.set({ name: 'session', slot: LEAD });
    await until(() => document.querySelectorAll('.conv .turn').length > 0);
    expect(
      document.querySelector('.sess .nm')?.textContent,
      'precondition: the route never came back to the lead',
    ).toBe('proj');

    // Frames are written synchronously and land a hop later, so the window a
    // frame would have taken is what makes "nothing was sent" a claim.
    await settle();
    expect(seatSubscribesOf(LEAD), 'the return subscribed the seat again').toBe(1);
    expect(
      seatUnsubscribesOf(LEAD),
      'the switch gave the seat back, so the return had to ask again',
    ).toBe(0);
  });

  /**
   * **A reconnect leaves the column on the seat it draws, and the next switch
   * still moves it.**
   *
   * The report that started this came from a client open for hours - long
   * enough for its socket to have dropped and come back unattended - and a
   * reconnect is the one thing none of the clean paths has inside it: a
   * re-subscribe and a fresh snapshot landing against state the client
   * already holds. Both halves are checked, because the reconnect alone is
   * the smaller failure if it is the trigger: the column must keep drawing
   * its seat, and a switch after it must move.
   */
  it('keeps drawing its seat through a reconnect, and still switches after one', async () => {
    const W1: SessionSlot = { ...LEAD, label: 'w1' };
    server = await stubServer(sessionFixture, {
      [subjectKey({ session: LEAD })]: [{ key: 'lead-t1', messages: [say('the lead said this')] }],
      [subjectKey({ session: W1 })]: [{ key: 'w1-t1', messages: [say('the worker said this')] }],
    });
    connection = connect(server.url);
    const route = writable<Route>({ name: 'session', slot: LEAD });
    app = mount(Routes, {
      target: document.body,
      props: { route, props: routerProps() },
    });
    await until(() => drawn().includes('the lead said this'));

    const before = seatSubscribesOf(LEAD);
    server?.drop();
    // The reconnect re-asks what the connection holds: one ask per seat, and
    // the seat is held once however many readers share it. **Waited past the
    // count the drop found, never on a number**: a fixed count is satisfied
    // before the drop even happens once the seat is subscribed twice, which is
    // the state this change removes - the assertion would then be reading the
    // pre-drop count and claiming a reconnect that never had to happen.
    await until(() => seatSubscribesOf(LEAD) > before);
    expect(seatSubscribesOf(LEAD), 'the reconnect did not re-ask the seat').toBe(before + 1);
    await until(() => drawn().includes('the lead said this'));
    expect(drawn(), 'the reconnect left the column without its conversation').toContain(
      'the lead said this',
    );

    route.set({ name: 'session', slot: W1 });
    await until(() => drawn().includes('the worker said this'));
    expect(drawn(), 'the switch after a reconnect kept the seat it left').not.toContain(
      'the lead said this',
    );
  });

  /* The queue-above-strip order is pinned where the column itself draws it:
     `Chat.test.ts` mounts the chat with a queue snippet and reads the two
     rows' document order. */

  /**
   * **A switch leaves the conversation of the seat it left behind**, through
   * the mount the app makes.
   *
   * The route is the shell's, and the page it draws is re-derived from it -
   * the header, the rail, the URL. The chat column is handed its seat through
   * the page's own snippet, so a switch that kept drawing the seat it left
   * says the reader is somewhere they are not, while everything around it
   * says otherwise. Reported live 2026-10-10, on the desktop app and the
   * browser both.
   */
  it('draws the seat the route moved to, not the one it left', async () => {
    const W1: SessionSlot = { ...LEAD, label: 'w1' };
    const say = (text: string): unknown => ({
      type: 'assistant',
      message: {
        id: `m-${text}`,
        role: 'assistant',
        model: 'claude-opus-5',
        content: [{ type: 'text', text }],
      },
    });
    // The worker's own row on the home: a seat the roster does not name is
    // the column's "not running" state, and the conversation is what this
    // case is about.
    const named = {
      ...homeWire,
      agents: [
        ...homeWire.agents,
        { ...(homeWire.agents[0] as object), slot: W1, label: 'w1' },
      ] as typeof homeWire.agents,
    };
    server = await stubServer(
      sessionFixture,
      {
        [subjectKey({ session: LEAD })]: [
          { key: 'lead-t1', messages: [say('the lead said this')] },
        ],
        [subjectKey({ session: W1 })]: [{ key: 'w1-t1', messages: [say('the worker said this')] }],
      },
      named,
    );
    connection = connect(server.url);
    const route = writable<Route>({ name: 'session', slot: LEAD });
    app = mount(Routes, {
      target: document.body,
      props: { route, props: routerProps(named) },
    });
    await settle();
    expect(drawn(), 'precondition: the first seat drew its conversation').toContain(
      'the lead said this',
    );

    route.set({ name: 'session', slot: W1 });
    await until(() => drawn().includes('the worker said this'));

    expect(drawn(), 'the chat kept drawing the conversation of the seat it left').not.toContain(
      'the lead said this',
    );
    expect(drawn(), 'the seat the route moved to never drew').toContain('the worker said this');
  });

  /**
   * **Below the fold width the rail parks, whatever the preference.** The
   * column is a wide-page shape; a folded page draws the rail off-canvas
   * and slides it over when the chip, Cmd+Left or the palette opens it.
   */
  it('keeps the rail parked at every width until it is summoned', async () => {
    matchMediaTo(true);
    await open(sessionFixture);

    const app = document.querySelector('.app')?.className ?? '';
    expect(app, 'the rail claimed a column on a narrow page').not.toContain('rail-open');
    expect(app, 'the width did not fold the column').not.toContain('rail-static');
    expect(document.querySelector('.rail.left'), 'the rail is drawn, to summon').not.toBeNull();
    expect(document.querySelector('.rail-tog.tog-r'), 'the inspector handle is gone').toBeNull();
  });

  /**
   * **The default is the column**: a wide page carries the rail statically,
   * with the pin to float it. The drawer's own content - the working rows,
   * the spend and the versions - is on the left from arrival.
   */
  it('keeps the projects rail as the column by default', async () => {
    // The stub is global once installed by the fold test above, so the wide
    // reading is asked for explicitly rather than assumed.
    matchMediaTo(false);
    await open(sessionFixture);

    const app = document.querySelector('.app')?.className ?? '';
    expect(app, 'the default did not pin the rail').toContain('rail-static');
    expect(document.querySelector('.banner .pin'), 'the pin is the way to float it').not.toBeNull();
  });

  /**
   * **The palette driven from the page itself.** Nothing else in the suite
   * presses the real key or reads history, which is how a stranded entry
   * survived a round: Cmd+K opens it, the walk starts on THIS org's lead (a
   * namesake in another org must not take it), the arrows move, and one Back
   * closes what it opened.
   */
  it('opens the palette from the page, walks it, and gives one Back', async () => {
    const base = homeWire.agents[0];
    if (base === undefined) throw new Error('the fixture holds no agent');
    await open(sessionFixture, {
      ...homeWire,
      agents: [base, { ...base, slot: { ...base.slot, org: 'Other' } }],
    });

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', metaKey: true }));
    flushSync();
    expect(document.querySelector('.pal'), 'Cmd+K drew no palette').not.toBeNull();
    expect(document.querySelector('.it.sel')?.id, 'the walk started on the wrong org lead').toBe(
      'pal-row-TestOrg/proj/lead',
    );

    document
      .querySelector('.pal input')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();
    expect(document.querySelector('.it.sel')?.id, 'ArrowDown did not walk').toBe(
      'pal-row-Other/proj/lead',
    );

    history.back();
    await until(() => document.querySelector('.pal') === null);
    expect(document.querySelector('.pal'), 'Back left the palette up').toBeNull();
  });

  /**
   * **The peek's entry becomes the rail's** - the round's finding: one Back
   * closes the rail, rather than the peek stranding a forward entry under a
   * pushed one and the next Back leaving the page.
   */
  it("hands the palette's peek entry to the rail, so one Back closes", async () => {
    matchMediaTo(true);
    await open(sessionFixture);
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', metaKey: true }));
    flushSync();
    const field = document.querySelector<HTMLInputElement>('.pal input');
    if (field === null) throw new Error('the palette drew no input');
    field.value = 'peek at the fleet';
    field.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    field.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    flushSync();

    expect(document.querySelector('.pal'), 'the peek left the palette up').toBeNull();
    expect(
      document.querySelector('.app')?.className ?? '',
      'the peek did not open the rail',
    ).toContain('rail-open');
    const state = history.state as { forgeRail?: string } | null;
    expect(state?.forgeRail, "the entry on top is not the rail's").toBe('left');

    history.back();
    await until(() => !(document.querySelector('.app')?.className ?? '').includes('rail-open'));
    expect(
      document.querySelector('.app')?.className ?? '',
      'one Back did not close the rail',
    ).not.toContain('rail-open');
  });

  /**
   * **The folded rail keeps its pointer door, whatever the stored mode.** A
   * static preference at a folded width must still answer the chip's hover:
   * the guard is the fold, not the preference - and nothing else in the
   * suite dispatches this pointer.
   */
  it('summons the folded rail on the chip hover', async () => {
    matchMediaTo(true);
    await open(sessionFixture);
    expect(
      document.querySelector('.app')?.className ?? '',
      'the folded rail opened by itself',
    ).not.toContain('rail-open');

    document
      .querySelector('.needchip')
      ?.dispatchEvent(new PointerEvent('pointerenter', { pointerType: 'mouse' }));
    flushSync();
    expect(
      document.querySelector('.app')?.className ?? '',
      'the folded rail ignored the chip hover',
    ).toContain('rail-open');
  });

  /**
   * **A Back returns the keyboard where the palette took it**: the pop path
   * restores focus the same way Escape and Cmd+K do.
   */
  it('returns the keyboard to the opener when a Back closes the palette', async () => {
    await open(sessionFixture);
    const chip = document.querySelector<HTMLElement>('.needchip');
    chip?.focus();
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', metaKey: true }));
    flushSync();
    expect(document.querySelector('.pal'), 'Cmd+K drew no palette').not.toBeNull();

    history.back();
    await until(() => document.querySelector('.pal') === null);
    expect(document.activeElement, 'the pop left the keyboard nowhere').toBe(chip);
  });

  /**
   * **Escape's route consumes its own entry**: one Back after an
   * Escape-closed palette must not find a stranded step - the same class R1
   * named, through the other door.
   */
  it('consumes the palette entry when Escape closes it', async () => {
    await open(sessionFixture);
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', metaKey: true }));
    flushSync();
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    await until(() => document.querySelector('.pal') === null);

    const left = (): string | undefined =>
      (history.state as { forgeRail?: string } | null)?.forgeRail;
    await until(() => left() === undefined);
    expect(left(), "Escape's route stranded its entry").toBeUndefined();
  });

  /**
   * **The answering role is declared when, and only when, the page can
   * answer.** The dock that answers a prompt lives in the composer, so a page
   * without one must subscribe as an observer: a client counted as able to
   * answer a prompt it cannot display hangs the turn, which is worse than the
   * cancel an observer gets.
   */
  it('subscribes as an observer while it has no dock to answer from', async () => {
    await open(sessionFixture);
    expect(seatSubscribe()?.answering, 'the page claimed an ability it has not got').toBe(false);
  });

  /**
   * The role flips with the presence of a dock, and nothing else moves.
   *
   * The snippet here is a RAW one, which is a harness convenience rather than
   * the shape the integration uses: the seam is written for a `{#snippet box(p)}`
   * in markup, where `p` arrives as the props object. `createRawSnippet` gets
   * its parameters by Svelte's internal convention instead, so this asserts the
   * role and that the box draws, and leaves the props to the site that binds
   * them.
   */
  it('declares the answering role once the composer is wired in', async () => {
    const composer = createRawSnippet(() => ({ render: () => '<span class="box"></span>' }));
    await open(sessionFixture, homeWire, { composer });

    expect(seatSubscribe()?.answering, 'a page with a dock subscribed as an observer').toBe(true);
    expect(drawn(), 'the composer did not draw').toContain('class="box"');
  });

  /**
   * **The count is the property, and it is a pair per visit.** The
   * subscription follows the page - a held one reads as a SHOWING seat to
   * the server, and showing spends the marks a seat earns - so a visit, a
   * leave and a return is a subscribe, an unsubscribe and a subscribe. What
   * the client keeps is the record, which is what the return draws from.
   */
  it('subscribes on each visit and gives the seat back between them', async () => {
    await open(sessionFixture);
    expect(seatSubscribes(), 'precondition: the first visit subscribed the seat').toBe(1);

    await leave();
    // The leave sends its unsubscribe over a real socket, so the frame has
    // a hop to make before the server has it.
    await settle();
    expect(server?.gone.length, 'the leave took the seat away from the socket').toBe(1);

    revisit();
    await settle();

    expect(seatSubscribes(), 'the return subscribed the seat again').toBe(2);
  });

  /**
   * A returning page draws from what is held rather than waiting on an answer,
   * which is the burst a return used to cost. Nothing is flushed here beyond
   * the mount: a page that needed the server to answer would draw nothing
   * until a socket round trip had been and come back.
   */
  it('draws a returning page from the record it already holds', async () => {
    await open(sessionFixture);
    await leave();
    revisit();
    flushSync();

    expect(git.strip()?.label, 'the return drew nothing until the server answered again').toContain(
      'worktree-pr',
    );
  });

  /**
   * **A seat's role only ever rises, and a page that gains its dock later has
   * to say so.** Every seat the app makes is subscribed answering, so this is
   * the path a seat subscribed as an observer takes when a page with a dock
   * arrives at it - and without it that page keeps the observer's role, which
   * cancels every prompt it shows rather than hanging a turn.
   */
  it('re-asks a seat under the answering role when a page arrives with a dock', async () => {
    await open(sessionFixture);
    expect(seatSubscribe()?.answering, 'precondition: the first visit had no dock').toBe(false);

    await leave();
    const composer = createRawSnippet(() => ({ render: () => '<span class="box"></span>' }));
    revisit({ composer });
    await settle();

    expect(seatSubscribes(), 'the second visit did not re-ask under the stronger role').toBe(2);
    expect(
      server?.received[1]?.answering,
      'the escalation did not declare the answering role',
    ).toBe(true);
    // The leave between the visits gave the seat back (the subscription
    // follows the page), and the escalation is the return's own subscribe -
    // so the one unsubscribe here is the leave's, not the escalation's.
    expect(server?.gone.length, 'the escalation gave a subscription back of its own').toBe(1);
  });
});

/** Answer every media query with `matches`, as a narrow window would. */
function matchMediaTo(matches: boolean): void {
  (globalThis as { matchMedia?: unknown }).matchMedia = () => ({
    matches,
    addEventListener: () => {},
    removeEventListener: () => {},
  });
}

/**
 * The painted frames a page waits for, run by hand.
 *
 * **The waiting is half of what these cases are about**, so a case has to be
 * able to say when a frame paints. jsdom does provide `requestAnimationFrame` -
 * vitest builds its window with `pretendToBeVisual` - but on a timer of its
 * own, so a publish would land whenever that clock said. The queue here is the
 * test's, and `paint()` is the paint.
 *
 * The stub is the whole file's, so a case anywhere in it that lands an update
 * and then reads the page needs `paint()`: the socket cases below land none
 * today and so never paint.
 */
let frames = new Map<number, () => void>();
let nextFrame = 1;

/** Take the global frame callbacks over, so `paint()` is the only clock. */
function stubFrames(): void {
  frames = new Map();
  nextFrame = 1;
  const global = globalThis as {
    requestAnimationFrame?: (callback: FrameRequestCallback) => number;
    cancelAnimationFrame?: (id: number) => void;
  };
  global.requestAnimationFrame = (callback) => {
    const id = nextFrame;
    nextFrame += 1;
    frames.set(id, () => callback(0));
    return id;
  };
  global.cancelAnimationFrame = (id) => {
    frames.delete(id);
  };
}

/** Paint one frame: everything waiting for one runs, in the order it asked. */
function paint(): void {
  const waiting = [...frames.values()];
  frames.clear();
  for (const run of waiting) run();
}

beforeEach(stubFrames);

// A test that fails part-way through a fake-timer case would otherwise leave
// every later case in this file on fake time - the twin carries the same
// guard for the same reason (`chat/conversation.test.ts`).
//
// The spies are restored here for the same reason: a stubbed `window.open`
// left behind by a red case would be in place for every case after it.
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

/**
 * One seat's stream, without a socket.
 *
 * **The page's own work is the question here**, and a real socket cannot be
 * asked it: what matters is whether the page asked for a read, and what it did
 * with what it heard. So this answers both - every ask is counted, and a test
 * lands a message by hand.
 */
interface Watching {
  /** Deliver one message, as the connection would. */
  land(message: ServerMessage): void;
  /** Move the connection through one state, as a drop and its reconnect do. */
  wentTo(next: ConnectionStatus): void;
  /** How many times the page asked the server for the session again. */
  reads(): number;
  /**
   * How many times the store handed its readers the record.
   *
   * A subscribe is handed the value the store holds, so the count starts at
   * one and a case reads the rest against a baseline it took itself.
   */
  publishes(): number;
  /** What the page currently holds. */
  read(): SessionRead;
  stop(): void;
}

function watch(connection: Driveable, slot: SessionSlot = LEAD): Watching {
  const view = watchSession(connection, slot, true);
  let publishes = 0;
  const stop = view.subscribe(() => {
    publishes += 1;
  });
  return {
    land: (message) => connection.land(message),
    wentTo: (next) => connection.wentTo(next),
    reads: () => connection.reads(),
    publishes: () => publishes,
    read: () => get(view),
    stop,
  };
}

/**
 * A connection a test drives by hand, answering the shape the page uses of one.
 *
 * The other members are present because the page takes a whole `Connection`:
 * a page that dispatches a command or asks for a page of turns is not what any
 * of these cases exercises, so they are the quietest thing that satisfies the
 * type and a test that reaches one fails loudly rather than silently.
 */
interface Driveable extends Omit<Connection, 'status'> {
  land(message: ServerMessage): void;
  wentTo(next: ConnectionStatus): void;
  reads(): number;
  /** How many times a seat was subscribed on this connection. */
  subscribes(): number;
  /** How many subscriptions were given back. */
  unsubscribes(): number;
  /** The seat's session appears, so a later read answers with a record. */
  ready(): void;
  status(): ConnectionStatus;
}

/** A seat the server holds no session for, worded as it words the refusal. */
const NO_SESSION = 'forge holds no session for this seat';

function drivable(refusedAtFirst = false): Driveable {
  const listeners = new Set<(message: ServerMessage) => void>();
  const watchers = new Set<(status: ConnectionStatus) => void>();
  const snapshot = new Map<string, unknown>();
  let reads = 0;
  let subscribed = 0;
  let unsubscribed = 0;
  let refused = refusedAtFirst;

  const seatState = (): StoreState =>
    refused ? { kind: 'refused', why: NO_SESSION } : { kind: 'ready' };

  const store = (subject: Subject): Store => {
    const key = subjectKey(subject);
    return {
      subject,
      value: writable<StoreValue>({
        snapshot: null,
        updates: [],
        state: { kind: 'ready' },
        dropped: 0,
      }),
      snapshot: () => snapshot.get(key) ?? null,
      updates: () => [],
      state: seatState,
      dropped: () => 0,
      set: (next: unknown) => {
        snapshot.set(key, next);
      },
      push: () => {},
      refuse: () => {},
    };
  };

  return {
    subscribe: (what) => {
      subscribed += 1;
      snapshot.set(subjectKey(what), null);
      return store(what);
    },
    unsubscribe: () => {
      unsubscribed += 1;
    },
    refresh: () => {
      reads += 1;
    },
    dispatch: () => {
      throw new Error('this page dispatched a command the case did not expect');
    },
    more: () => false,
    devices: () => false,
    frame: () => false,
    onBrowserAsk: () => () => {},
    browserRole: () => false,
    onBrowserRole: () => () => {},
    takeBrowserRole: () => {},
    onMessage: (fn) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (fn) => {
      watchers.add(fn);
      return () => watchers.delete(fn);
    },
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => 'open',
    close: () => {},
    land: (message) => {
      if (message.kind === 'snapshot') {
        snapshot.set(subjectKey(message.subject), message.data);
      }
      for (const fn of listeners) fn(message);
    },
    wentTo: (next) => {
      for (const fn of watchers) fn(next);
    },
    reads: () => reads,
    subscribes: () => subscribed,
    unsubscribes: () => unsubscribed,
    ready: () => {
      refused = false;
    },
  };
}

/** The seat's own answer, with whatever a case wants carried on it. */
function snapshotOf(slot: SessionSlot, extra: Record<string, unknown> = {}): ServerMessage {
  return {
    kind: 'snapshot',
    subject: { session: slot },
    data: { slot, state: { scan_cwd: '/tmp' }, ...extra },
  };
}

/** One update, as the server sends it. */
function updateOf(update: SessionUpdate): ServerMessage {
  return { kind: 'update', update };
}

/** The seat taking a new occupant, which is a read rather than a patch. */
function occupant(sessionId: string): SessionUpdate {
  return {
    connected: {
      key: LEAD,
      session_id: sessionId,
      cwd: '/tmp',
      current_model: null,
      available_models: [],
      mode: null,
      history: [],
      compaction_count: 0,
    },
  };
}

/** The same occupant arriving under another name the seat is replaced by. */
function occupantAs(name: string): SessionUpdate {
  return { [name]: Object.values(occupant('new'))[0] };
}

/** A prompt as the CLI writes one: the frame shape the dev fixture carries. */
function spoke(text: string, uuid = 'u1'): Record<string, unknown> {
  return {
    type: 'user',
    uuid,
    message: { role: 'user', content: [{ type: 'text', text }] },
    session_id: 's',
  };
}

/**
 * Every word a record's turns carry, in the order its frames arrived.
 *
 * The record holds wire frames as they came, typed `unknown`, so the shape
 * `spoke` built is narrowed here rather than restated by the production types.
 */
function spoken(page: Watching): string[] {
  return (page.read().wire?.conversation.turns ?? []).flatMap((turn) =>
    turn.messages.map((frame) => {
      const content = (frame as { message?: { content?: { text?: string }[] } }).message?.content;
      return (content ?? []).map((part) => part.text ?? '').join('');
    }),
  );
}

describe('the record a page holds over an update stream', () => {
  it('applies an update for this seat instead of asking for the session again', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const before = page.read().wire;
    const asked = page.reads();

    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();

    expect(page.read().wire, 'the update never reached the record').not.toBe(before);
    expect(page.read().wire?.conversation.turns, 'the frame is not in the record').toHaveLength(1);
    expect(page.reads(), 'the page asked for a read on an update').toBe(asked);
    page.stop();
  });

  /**
   * A paint that never comes must not stop the record either.
   *
   * Same flag, same callback-only clear as the chat's `soon()` - this is its
   * twin, and a browser that drops one scheduled callback (a suspended page,
   * a locked screen) must cost one throttled write, not every later frame.
   */
  it('a paint that never comes does not stop the record', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));

    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    expect(
      page.read().wire?.conversation.turns,
      'the update waits for the paint that is owed it',
    ).toHaveLength(0);

    vi.advanceTimersByTime(1_000);

    expect(
      page.read().wire?.conversation.turns,
      'the watchdog writes it without the paint',
    ).toHaveLength(1);

    // And the deadline is spent, not just fired: a watchdog that wrote without
    // clearing the flag would land this one and re-wedge on the next frame.
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('after the deadline', 'u2') } }));
    vi.advanceTimersByTime(1_000);
    expect(spoken(page), 'and the next frame lands too').toContain('after the deadline');

    // The other half of spending it: a paint that lands clears the deadline,
    // so the stale timer must not write a second time behind it.
    page.land(
      updateOf({
        chat_appended: { key: LEAD, msg: spoke('painted, and no timer behind it', 'u3') },
      }),
    );
    paint();
    expect(spoken(page), 'the paint lands it').toContain('painted, and no timer behind it');
    const settled = page.publishes();
    vi.advanceTimersByTime(1_000);
    expect(page.publishes(), 'and the spent deadline writes nothing more').toBe(settled);
    page.stop();
  });

  /**
   * **A fired cron's schedule is on the frame and on no field of the record.**
   * The row's prose carries the prompt alone, so the pump keeps the pairing
   * the frame states - and it has to do so where the record's own early return
   * cannot skip it, since this update is one the record ignores.
   */
  it('keeps the schedule a fired cron named, which no record field carries', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const before = page.read().wire;

    page.land(
      updateOf({
        cron_prompt_appended: {
          key: LEAD,
          text: 'summarise overnight CI',
          uuid: 'p-cron-1',
          cron_id: 'c1',
          description: 'Morning summary',
        },
      }),
    );
    paint();

    expect(cronNames.nameFor('p-cron-1'), 'the frame named its schedule').toBe('Morning summary');
    expect(page.read().wire, 'and the record itself is untouched by it').toBe(before);
    page.stop();
  });

  /**
   * **A burst of updates is one redraw.** The record moves on every frame, and
   * a page is handed the record once per painted frame - so a stream arriving
   * faster than the display refreshes costs one redraw rather than one per
   * update, with nothing dropped and the order kept.
   */
  it('publishes a burst of updates as one painted frame', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const published = page.publishes();

    for (const word of ['one', 'two', 'three']) {
      page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke(word) } }));
    }

    expect(page.publishes(), 'a frame published before the paint').toBe(published);
    paint();
    expect(page.publishes(), 'the burst cost a publish per update').toBe(published + 1);
    expect(spoken(page), 'the burst was not published whole, or not in order').toEqual([
      'one',
      'two',
      'three',
    ]);

    // And the queue is armed again by the next frame rather than spent on the
    // first paint: a stream is many of these, not one.
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('four') } }));
    paint();
    expect(page.publishes(), 'the queue published once and stopped').toBe(published + 2);
    expect(spoken(page), 'a frame after the first paint was lost').toEqual([
      'one',
      'two',
      'three',
      'four',
    ]);
    page.stop();
  });

  /**
   * **A settings pick moves the record the moment the core echoes it.** The
   * update carries the whole set and the seat folds it the way the terminal's
   * own arm folds it, so the panel follows the pick instead of waiting for a
   * read to carry the record past it.
   */
  it("leaves an override echo alone: the axes are this client's own now", () => {
    // The core echoes `dictate_overrides` for the terminal's overlay; this
    // client holds its own axes, so the echo is an update with nothing here
    // to write it to - and it must not cost a read either.
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const asked = page.reads();
    const before = page.read().wire;

    page.land(
      updateOf({
        dictate_overrides: {
          key: LEAD,
          overrides: { styling: 'formal', structure: 'lists', context: null },
        },
      }),
    );
    paint();

    expect(page.read().wire, 'the echo wrote nothing').toBe(before);
    expect(page.reads(), 'and it cost no read').toBe(asked);
    page.stop();
  });

  it('leaves the record alone for another seat sent over the same connection', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const held = page.read().wire;

    page.land(
      updateOf({
        chat_appended: {
          key: { org: 'Busytools', project: 'forge', label: 'other' },
          msg: spoke('hi'),
        },
      }),
    );

    expect(page.read().wire, "another seat's frames reached this page").toBe(held);
    page.stop();
  });

  it('holds an update that arrives before the first read', () => {
    const connection = drivable();
    const page = watch(connection);

    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));

    expect(page.read().wire, 'an update invented a record out of nothing').toBeNull();
    expect(page.read().refused).toBeNull();
    page.stop();
  });

  it('replaces the record wholesale when the connection comes back', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();
    expect(page.read().wire?.conversation.turns, 'precondition: the frame landed').toHaveLength(1);

    page.wentTo('closed');
    page.wentTo('open');
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    expect(
      page.read().wire?.conversation.turns,
      'the reconnect did not replace what the drop left behind',
    ).toHaveLength(0);
    page.stop();
  });

  it('asks for a whole record for every name that replaces the seat', () => {
    // The four are one property - a seat that wakes, connects, takes a new
    // occupant, or is handed a history cannot be answered by the record the
    // last one left - and the branch that reads them is what a page's whole
    // history hangs on.
    //
    // **The names are written out rather than read off `REPLACES`.** A loop
    // over the list under test cannot see the list change: a name dropped from
    // it takes its own coverage with it, and every test stays green while a
    // `/new` or a `/resume` stops being re-read.
    const replacing = ['spawning', 'connected', 'history_replayed', 'session_replaced'];
    expect(
      [...REPLACES].sort(),
      'the list the page reads is not the list this test covers',
    ).toEqual([...replacing].sort());

    for (const name of replacing) {
      const connection = drivable();
      const page = watch(connection);
      page.land(snapshotOf(LEAD));
      const asked = page.reads();

      page.land(updateOf(occupantAs(name)));

      expect(page.reads(), `${name} was never re-read`).toBe(asked + 1);
      page.stop();
    }
  });

  /**
   * **A page leaving gives its subscription back, and the record it held
   * stays.** A held subscription reads as a SHOWING seat to the server, and
   * showing spends every mark the seat earns - the failure mark and the
   * diamond both - so the subscription follows the page while the record
   * stays for the return to draw until the return's own subscribe answers
   * with the whole seat.
   */
  it('gives the subscription back when the last reader leaves, keeping the record', () => {
    const connection = drivable();
    const away = watch(connection);
    away.land(snapshotOf(LEAD));
    away.stop();
    expect(connection.unsubscribes(), 'the leave gave the seat back').toBe(1);

    const back = watch(connection);
    expect(connection.subscribes(), 'and the return subscribed it again').toBe(2);
    expect(back.read().wire, 'the held record draws until the answer lands').not.toBeNull();
    back.stop();
    expect(connection.unsubscribes(), 'the second leave gives it back too').toBe(2);
  });

  /**
   * **A refused seat a conversation holds keeps that hold.** The refusal gives
   * the subscription back and clears the record, but the seat itself stays -
   * and this is why: forgotten, the next visit builds a seat only the page
   * holds, and that page's leave takes the subscription away with the
   * conversation still folding it.
   */
  it('keeps a refused seat for the holder that outlives the page', () => {
    const connection = drivable(true);
    const conversation = watchSession(connection, LEAD, false).hold();

    // A page meets the refusal and leaves: the subscription goes back, and the
    // seat stays, because the conversation's hold is on it.
    const first = watch(connection);
    first.land(snapshotOf(LEAD));
    first.stop();
    expect(connection.unsubscribes(), 'the refused visit gave its subscription back').toBe(1);

    // The seat starts, a page visits it and leaves again. The conversation is
    // still holding it, so its subscription stays where it is.
    connection.ready();
    const second = watch(connection);
    second.land(snapshotOf(LEAD));
    const before = connection.unsubscribes();
    second.stop();
    expect(
      connection.unsubscribes(),
      'the page took the seat away from the conversation still holding it',
    ).toBe(before);

    conversation();
  });

  /**
   * **The escalation opens the seat's subscription, never a tab.** The arm
   * that raises the role lives outside the seat's own factory, where a bare
   * `open()` is the DOM global: the role would rise with nothing subscribed,
   * and every gate would stay green - `open()` with no arguments is valid
   * TypeScript, and a page that does subscribe opens the seat one step later
   * anyway.
   *
   * The case that catches it is the one the seat's own doc names: a caller
   * that takes the store and never subscribes, so the escalation is the only
   * opener there is.
   */
  it('opens the seat itself when the answering role arrives', () => {
    const windowed = vi.spyOn(window, 'open').mockImplementation(() => null);
    const connection = drivable();
    watchSession(connection, LEAD, false);
    watchSession(connection, LEAD, true);

    expect(connection.subscribes(), 'the escalation opened nothing').toBe(1);
    expect(windowed, 'the escalation called the DOM global').not.toHaveBeenCalled();
  });

  /**
   * A replacement frame for a seat nobody is showing stays out of the
   * socket. The ask would be an unsubscribe-then-subscribe pair, and the
   * subscribe re-attaches a seat no page is showing: the server reads it
   * as shown - spending the marks every reader would get - with no counter
   * here owning it. The return's own subscribe answers with the whole
   * record, which covers everything the away ask was for.
   */
  it('asks nothing for an away seat when a replacement frame lands', () => {
    const connection = drivable();
    const away = watch(connection);
    away.land(snapshotOf(LEAD));
    away.stop();
    const asked = connection.reads();

    connection.land(updateOf(occupant('new-occupant')));

    expect(connection.reads(), 'the away replacement frame asked the server again').toBe(asked);
    expect(connection.subscribes(), 'and it re-subscribed an away seat').toBe(1);
    expect(connection.unsubscribes(), 'and it gave one back to do it').toBe(1);
  });

  /**
   * **A record still waiting for a frame is handed over when the page goes.**
   * No frame paints for a page that has left, so a frame that arrived before
   * the leave and would have been published at the next paint is published by
   * the leave itself - and that is the record a return draws.
   */
  it('hands a record waiting for a frame over when the page leaves', () => {
    const connection = drivable();
    const showing = watch(connection);
    showing.land(snapshotOf(LEAD));
    showing.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('held') } }));
    // A frame was waiting for a paint when the page left.
    showing.stop();

    const back = watch(connection);
    // The paint that follows has nothing left to write: the leave took the
    // waiting frame out of the queue along with the record it carried. The one
    // write counted here is the subscribe's own handing over.
    paint();
    expect(back.publishes(), 'the leave left a frame armed to paint again').toBe(1);
    expect(spoken(back), 'the frame the leave cut short was lost').toEqual(['held']);
    back.stop();
  });
});

/**
 * The seat between visits: what the client holds, what it asks for again, and
 * what it must not.
 *
 * **The client owns a seat's state, and a seat is not the page drawing it.**
 * The terminal has done this for forge's life - one subscribe, every seat's
 * record held, and moving between sessions changing what it draws and nothing
 * else - so these are the cases that hold the same property here.
 */
describe('the seat the client holds between visits', () => {
  /**
   * A subscription lives on one socket, so a client that moved to another
   * address holds a seat on each. A registry keyed on the slot alone hands the
   * new connection the old connection's record, which then draws whatever the
   * seat said before the move and takes no update from the socket now carrying
   * it.
   */
  it('holds a seat per connection rather than per slot', () => {
    const a = drivable();
    const b = drivable();
    const onA = watch(a);
    onA.land(snapshotOf(LEAD));
    expect(onA.read().wire, 'precondition: the first connection holds a record').not.toBeNull();

    const onB = watch(b);

    expect(b.subscribes(), 'the second connection never subscribed the seat itself').toBe(1);
    expect(onB.read().wire, "one connection's record was drawn from another connection").toBeNull();
    onB.stop();
    onA.stop();
  });

  /**
   * **A refusal gives its subscription back, and the next visit opens the
   * seat afresh.** The server watches nothing for a seat it refused - "a
   * refused subscribe leaves nothing to hear" - so the record is cleared and
   * the return subscribes again, which is what a seat that starts later
   * needs. The seat itself is kept; what it does not keep is a subscription
   * that was never granted.
   */
  it('opens a refused seat afresh on the next visit', () => {
    const connection = drivable(true);
    const first = watch(connection);
    expect(first.read().refused, 'precondition: the server refused the seat').toBe(NO_SESSION);
    first.stop();

    const back = watch(connection);

    expect(
      connection.subscribes(),
      'a seat the server held nothing behind was kept as though it were subscribed',
    ).toBe(2);
    expect(back.read().refused, 'the return drew nothing of the refusal').toBe(NO_SESSION);
    back.stop();
  });

  /**
   * **A refusal is cleared with the visit that met it, not carried.**
   *
   * It is an answer rather than a record, so the leave writes it away and the
   * next visit is handed a cold load rather than the refusal it met. **Read
   * off the return's own subscription, and off its FIRST value**: the leave's
   * write goes to whoever comes next, and the subscriber that left is already
   * gone by the time it lands. The seat is ready by then, so nothing re-reads
   * over the top of it - a read of a ready store with no snapshot yet
   * publishes nothing, which is what leaves the cleared value to be seen.
   */
  it('clears a refusal with the visit that met it', () => {
    const connection = drivable(true);
    const first = watch(connection);
    expect(first.read().refused, 'precondition: the server refused the seat').toBe(NO_SESSION);
    first.stop();
    connection.ready();

    const seen: (string | null)[] = [];
    const back = watchSession(connection, LEAD, true);
    const stop = back.subscribe((read) => seen.push(read.refused));

    expect(seen[0], 'the return drew the refusal the last visit met').toBeNull();
    stop();
  });

  /**
   * A drop and its reconnect are the seat's, not the page's: the reconnect
   * answers with a snapshot of its own, and it replaces the record rather than
   * merging into it. Left to the page, a drop nobody was showing leaves the
   * pacing armed for the pre-drop read, and the reconnect's whole record is
   * merged away - the previous occupant's conversation standing as this one's.
   */
  it('replaces the record on a reconnect even when nobody was showing the seat', () => {
    const connection = drivable();
    const away = watch(connection);
    away.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    away.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();
    expect(away.read().wire?.conversation.turns, 'precondition: the frame landed').toHaveLength(1);
    away.stop();

    connection.wentTo('closed');
    connection.wentTo('open');
    connection.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    const back = watch(connection);
    expect(
      back.read().wire?.conversation.turns,
      'the reconnect merged into the record the drop left instead of replacing it',
    ).toHaveLength(0);
    back.stop();
  });
});

/**
 * The read this client still makes, and the one answer it refuses.
 *
 * **Every slice a frame can carry has a handler now**, so a read is the whole
 * record and there is no field a timer keeps fresh: the poll retired with the
 * last one. What is left to pin is when an ANSWER is taken - a cold load, a
 * reconnect and a swap are owed the whole record, and an answer the frames
 * have already outrun is not.
 */
describe('the read, and the answer a frame has outrun', () => {
  /**
   * **A seat shown again draws what it holds and asks for nothing.** The
   * record it kept is the one the frames kept fresh, so a return is a draw
   * rather than a read - the burst this whole series exists to delete.
   */
  it('draws a seat shown again without asking for it', () => {
    const connection = drivable();
    const showing = watch(connection);
    showing.land(snapshotOf(LEAD));
    showing.stop();

    const back = watch(connection);
    // The seat is answered as it would be on a socket, where the reconnect
    // that carried the page back also answered the subscribe.
    back.land(snapshotOf(LEAD));
    const asked = connection.reads();
    expect(asked, 'the return read the seat rather than drawing what it holds').toBe(0);

    back.stop();
  });

  /**
   * **An answer the frames have outrun is refused, and asked for again.** A
   * read is the whole record now, so one encoded before a frame that has
   * already landed carries the seat backwards: the page keeps what the frames
   * left and asks a second time, which is the want the merge used to satisfy
   * by patching around the answer.
   */
  it('refuses an answer the frames have outrun, and asks again', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();
    expect(page.read().wire?.conversation.turns, 'precondition: the frame landed').toHaveLength(1);

    // The seat takes a new occupant: the page asks for the whole record.
    page.land(updateOf(occupant('new')));
    const asked = page.reads();

    // A frame lands while that answer is in flight, so the record has been
    // carried past where the answer was encoded.
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('more') } }));
    paint();

    // The answer lands: the seat as it was BEFORE that frame, with the
    // previous occupant's empty conversation in it.
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    expect(
      page.read().wire?.conversation.turns,
      'an answer the frames had outrun was taken',
    ).toHaveLength(2);
    expect(page.reads(), 'the outrun answer was never asked for again').toBe(asked + 1);

    // And the fresh ask converges: its clean answer is taken whole.
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 }, work: null }));
    expect(page.read().wire?.conversation.turns, 'the fresh answer was not taken').toHaveLength(0);
    page.stop();
  });

  /**
   * **A refusal names no subject, and this client now holds many seats.** The
   * socket hangs a refusal on the oldest ask the connection has outstanding,
   * and every held seat's listener is handed the error - so a seat whose own
   * store was not the refused one must leave its ask standing. Spent here, the
   * whole record a swap asked for is merged instead of taken, and the previous
   * occupant's conversation stands until the next replace or a reconnect.
   */
  it("does not spend another seat's refusal on its own whole-record ask", () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();
    page.land(updateOf(occupant('new')));
    expect(page.reads(), 'precondition: the swap asked for a whole record').toBe(1);

    // Another seat's subscribe refused, while this page's ask is in flight.
    page.land({ kind: 'error', what: 'subscribe', why: NO_SESSION });

    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    expect(
      page.read().wire?.conversation.turns,
      "another seat's refusal demoted the swap's whole record to a merge",
    ).toHaveLength(0);
    page.stop();
  });

  it('keeps a swap whole when an unrelated error lands before its answer', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    paint();

    page.land(updateOf(occupant('new')));
    // An error names no subject, so it is not this seat's record - and it must
    // not spend the answer the swap is waiting for.
    page.land({ kind: 'error', what: 'more', why: 'no page' });
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    expect(
      page.read().wire?.conversation.turns,
      'an error for something else let the swap answer be merged',
    ).toHaveLength(0);
    page.stop();
  });

  it('does not put a settled turn back from an answer older than the frame', () => {
    const connection = drivable();
    const page = watch(connection);
    // The seat's record says a turn is running.
    page.land(snapshotOf(LEAD, { header: { turn_in_flight: true } }));

    // A frame that replaces the record asks for a whole one, which is what a
    // reconnect or a swap does.
    page.land(updateOf(occupant('new')));
    const asked = page.reads();

    // The turn settles while that answer is in flight - a frame, and an
    // answer from before it would put the running turn back.
    page.land(updateOf({ turn_complete: { key: LEAD } }));
    paint();

    // The answer lands: the seat as it was BEFORE the settle. Adopted whole
    // it puts the running turn back, and the box refuses a send the turn has
    // already taken.
    page.land(snapshotOf(LEAD, { header: { turn_in_flight: true } }));

    expect(
      page.read().wire?.header.turn_in_flight,
      'an answer from before the settle put the running turn back',
    ).toBe(false);

    // **The whole record is still WANTED.** Refusing the outrun answer keeps
    // the frame-fed record, which is the fix above, but the conversation and
    // the header this answer was asked for are still owed - so the want is kept
    // and asked again. Without this the old occupant stands until a later
    // REPLACES frame with a clean window or a socket drop.
    expect(page.reads(), 'the whole record was not asked for again').toBe(asked + 1);

    // And the fresh ask converges: its clean answer is adopted whole.
    page.land(snapshotOf(LEAD, { header: { turn_in_flight: false }, work: { branch: 'new' } }));
    expect(page.read().wire?.work.branch, 'the fresh answer was not taken').toBe('new');
    page.stop();

    // The control, and it is the guard's NARROWNESS it holds: the same answer
    // with nothing outrunning it is adopted whole and does NOT ask again. An
    // over-broad guard re-asks here and fails this.
    const control = drivable();
    const other = watch(control);
    other.land(snapshotOf(LEAD, { header: { turn_in_flight: true } }));
    other.land(updateOf(occupant('new')));
    const askedOnce = other.reads();
    other.land(snapshotOf(LEAD, { header: { turn_in_flight: true }, work: { branch: 'new' } }));

    expect(other.read().wire?.work.branch, 'the control took the record').toBe('new');
    expect(other.reads(), 'a clean answer was asked for again').toBe(askedOnce);
  });

  it('does not read a take or a notice off the record', () => {
    const connection = drivable();
    const page = watch(connection);
    // A record from a server that still carried them: the narrowing reads
    // neither, because a take belongs to the connection that started it and
    // this client builds its own from that connection's updates.
    page.land(
      snapshotOf(LEAD, {
        composer: {
          take: { phase: 'recording', floor_db: -50, levels: [], peak_db: -50 },
          notice: { kind: 'landed', text: 'the words', truncated: false },
          compacting: false,
          sign_in: null,
        },
      }),
    );

    expect(
      page.read().wire?.composer.take,
      "a record-carried take is not this client's",
    ).toBeNull();
    expect(page.read().wire?.composer.notice).toBeNull();
    page.stop();
  });

  /**
   * **A whole-record answer carries no take, and the take the fold drew must
   * survive it.** The row is drawn off the record, so an answer that dropped
   * the take the page's own updates had drawn emptied the screen while the
   * recording stayed live - the vanishing recording of #1880.
   */
  it('keeps the take this connection started across a whole-record answer', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, {}));
    page.land(updateOf({ dictate_started: { key: LEAD, floor_db: -50, generation: 1 } }));
    paint();
    expect(page.read().wire?.composer.take, 'precondition: the fold drew the take').not.toBeNull();

    // The seat is left and returned to: the return's subscribe is answered
    // with a whole record, and a take is not on one. The answer differs from
    // the first record in a field of its own, so "the take was carried" and
    // "the answer was ignored" cannot read alike.
    page.land(snapshotOf(LEAD, { work: { branch: 'returned' } }));

    expect(page.read().wire?.work.branch, 'the answer itself was not taken').toBe('returned');
    expect(
      page.read().wire?.composer.take,
      'the recording vanished from the record',
    ).not.toBeNull();
    page.stop();
  });

  /**
   * A socket that leaves `open` takes its take with it: the core drops a take
   * whose reader went away, so a record that kept drawing one would be a
   * recording whose only offer is a refusal - and one no later answer clears,
   * because nothing about it will ever be said again.
   *
   * `connecting` is the status a real drop reports (`socket.ts`'s close
   * handler), so it is the one this must not miss.
   */
  it('drops the folded take when the socket leaves open', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, {}));
    page.land(updateOf({ dictate_started: { key: LEAD, floor_db: -50, generation: 1 } }));
    paint();
    expect(page.read().wire?.composer.take, 'precondition: a take is drawn').not.toBeNull();

    page.wentTo('connecting');
    paint();

    expect(
      page.read().wire?.composer.take,
      'a dropped socket left a phantom recording on the record',
    ).toBeNull();
    page.stop();
  });
});

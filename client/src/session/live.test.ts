// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { createRawSnippet, flushSync, mount, unmount } from 'svelte';
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

import { homeWire } from '../dev/fixture.data';
import sessionFixture from '../dev/fixtures/session.json';
import type { ServerMessage, SessionUpdate, Subject } from '../protocol';
import { subjectKey } from '../protocol';
import Router from '../shell/Router.svelte';
import { connect, type Connection, type ConnectionStatus } from '../socket';
import type { Store, StoreState, StoreValue } from '../stores';
import type { SessionSlot } from '../wire/types';
import { REPLACES } from './apply';
import { POLL_MS, watchSession, type SessionRead } from './live';
import Session from './Session.svelte';

// This is the one page mounted over a REAL connection, so it drives the
// column's store and status branches too - and its record is frozen where the
// class publishes it, like every other file that mounts the column.
vi.mock('../chat/conversation', async (importOriginal) => {
  const { frozenConversation } = await import('../chat/testing/frozen');
  return frozenConversation(await importOriginal<typeof import('../chat/conversation')>());
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
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** A forge that answers the two subjects a session page watches. */
async function stubServer(session: unknown) {
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
      version: 1,
      settings: { mark: null, theme: null, font: null },
    };
    socket.send(JSON.stringify(greeting));
    socket.on('message', (data) => {
      const message = JSON.parse(text(data)) as Subscribe;
      if (message.kind === 'unsubscribe') {
        gone.push(message);
        return;
      }
      if (message.kind !== 'subscribe') return;
      received.push(message);
      const snapshot: ServerMessage = {
        kind: 'snapshot',
        subject: message.what as ServerMessage extends { subject: infer S } ? S : never,
        data: message.what === 'home' ? homeWire : session,
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

describe('the session page over a socket', () => {
  it('draws the seat record the server answered with', async () => {
    await open(sessionFixture);

    expect(sections(), 'the seat record never reached the page').toContain('git');
    const facts = document.querySelector('.sess .facts');
    expect(facts?.textContent, 'the header drew no facts from the record').toContain('max');
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
   * **A Slack target crosses as a bare string.** `SlackSubscriptionTarget` is
   * an externally tagged enum, so `DirectMessages` and `Mentions` are strings
   * on the wire and only `Conversation` is an object; a narrow that reads all
   * three as objects misses both class arms and draws an empty row.
   */
  it('draws a slack target the wire sends as a bare string', async () => {
    await open(sessionFixture, slackWire());
    openSection('slack');

    const body = drawn();
    expect(sections()).toContain('slack');
    expect(body, 'a class target drew an empty key').toContain('mentions anywhere');
    expect(body).toContain('direct messages');
    // And the mode follows the target rather than defaulting to every message.
    expect(body).toContain('mentions only');
  });

  /**
   * Two subscriptions in one workspace can draw the SAME words - two mentions
   * watchers over one workspace is an ordinary thing to configure - so a row is
   * keyed by the subscription's own id. Keyed by the drawn words instead, the
   * two collide and Svelte throws, which is how this reached the browser. The
   * fixture carries such a pair, because a fixture whose three targets all read
   * differently would pass either way.
   */
  it('draws subscriptions that read the same words without colliding', async () => {
    await open(sessionFixture, slackWire());
    openSection('slack');
    expect(document.querySelectorAll('.sb .subs > li')).toHaveLength(4);
  });

  /**
   * The mount the app actually makes, asserted rather than walked by hand: the
   * Router's session branch is what puts the page on screen, and nothing else
   * in the suite renders it.
   */
  it('reaches the page through the Router a session URL lands on', async () => {
    server = await stubServer(sessionFixture);
    connection = connect(server.url);
    app = mount(Router, {
      target: document.body,
      props: {
        route: { name: 'session', slot: LEAD },
        settings: { mark: null, theme: null, font: null },
        address: '',
        home: { wire: homeWire, refused: null },
        failure: null,
        connected: true,
        connection,
        onconnect: () => {},
      },
    });
    await settle();

    expect(sections()).toContain('git');
    expect(drawn()).toContain('aria-label="inspector"');
  });

  /**
   * **The narrow bands are page state now, not checkboxes**, and jsdom has no
   * `matchMedia` at all, so both rails take the wide default in every other
   * test. This is the one case that sees the state the sheet folds by.
   */
  it('folds both rails away below the width they stop being columns at', async () => {
    matchMediaTo(true);
    await open(sessionFixture);

    // The sheet does the hiding, and jsdom performs no layout - so the page's
    // own state is the assertion, and its class is how that state is stated.
    const app = document.querySelector('.app')?.className ?? '';
    expect(app, 'the rail ignored a narrow page').toContain('left-hidden');
    expect(app).toContain('right-hidden');
    expect(document.querySelector('.rail-tog.tog-r')?.getAttribute('aria-expanded')).toBe('false');
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
   * **The count is the property.** A seat's subscription belongs to the client
   * rather than to the page showing it, so a visit, a leave and a return is
   * ONE subscribe and no unsubscribe - where a subscription owned by the page
   * makes a subscribe and an unsubscribe on every leg. Everything else here is
   * what that buys; this is the number it is bought with.
   */
  it('subscribes a seat once across a visit, a leave and a return', async () => {
    await open(sessionFixture);
    expect(seatSubscribes(), 'precondition: the first visit subscribed the seat').toBe(1);

    await leave();
    revisit();
    await settle();

    expect(seatSubscribes(), 'the return subscribed the seat a second time').toBe(1);
    expect(server?.gone, 'the leave took the seat away from the socket').toEqual([]);
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

    expect(sections(), 'the return drew nothing until the server answered again').toContain('git');
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
    expect(server?.gone, 'the escalation gave a subscription back').toEqual([]);
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

/** A home whose slack workspace holds a conversation, a DM class and mentions. */
function slackWire(): typeof homeWire {
  const project = homeWire.projects[0];
  if (project === undefined) throw new Error('the fixture holds no project');
  return {
    ...homeWire,
    connectors: {
      gotify: { connected: false, subscriptions: [] },
      slack: {
        connected_workspaces: [['Trust Machines', true]],
        load_failed: false,
        subscriptions: [
          {
            id: 's1',
            workspace: 'Trust Machines',
            target: { Conversation: { id: 'C1', name: '#alerts', mode: 'All' } },
          },
          { id: 's2', workspace: 'Trust Machines', target: 'DirectMessages' },
          { id: 's3', workspace: 'Trust Machines', target: 'Mentions' },
          // The same words as s3: a row keyed by the target rather than by the
          // subscription's own id throws on this pair.
          { id: 's4', workspace: 'Trust Machines', target: 'Mentions' },
        ],
      },
    },
  };
}

/** The `data-k` of every section the page drew, in the order it drew them. */
function sections(): string[] {
  return [...document.querySelectorAll('[data-k^="sec-"]')].map(
    (el) => el.getAttribute('data-k')?.slice('sec-'.length) ?? '',
  );
}

/**
 * Open a section, which is what a reader does before its body means anything.
 *
 * **A section draws its body when it is open and not before.** The summary is
 * all a closed section owes, so a test that reads a body has to open the thing
 * first - and this is the same pair the browser sends, the property and the
 * event `bind:open` listens for. jsdom does not implement `<summary>`
 * activation, so a `click` would toggle nothing here while looking like it had.
 */
function openSection(name: string): void {
  const found = document.querySelector(`details.sec[data-k="sec-${name}"]`);
  if (!(found instanceof HTMLDetailsElement)) throw new Error(`no ${name} section was drawn`);
  found.open = true;
  found.dispatchEvent(new Event('toggle'));
  flushSync();
}

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
  /** What the page currently holds. */
  read(): SessionRead;
  stop(): void;
}

function watch(connection: Driveable, slot: SessionSlot = LEAD): Watching {
  const view = watchSession(connection, slot, true);
  const stop = view.subscribe(() => {});
  return {
    land: (message) => connection.land(message),
    wentTo: (next) => connection.wentTo(next),
    reads: () => connection.reads(),
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
  status(): ConnectionStatus;
}

/** A seat the server holds no session for, worded as it words the refusal. */
const NO_SESSION = 'forge holds no session for this seat';

function drivable(refused = false): Driveable {
  const listeners = new Set<(message: ServerMessage) => void>();
  const watchers = new Set<(status: ConnectionStatus) => void>();
  const snapshot = new Map<string, unknown>();
  let reads = 0;
  let subscribed = 0;
  let unsubscribed = 0;

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

/**
 * A take's landed notice, as the record carries it.
 *
 * Opaque on purpose: it is the composer's own state and every reader leaves it
 * as it came - which is exactly why a stale copy of it can reach the box.
 */
function landed(): Record<string, unknown> {
  return { landed: true, text: 'the words', truncated: false };
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

describe('the record a page holds over an update stream', () => {
  it('applies an update for this seat instead of asking for the session again', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const before = page.read().wire;
    const asked = page.reads();

    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));

    expect(page.read().wire, 'the update never reached the record').not.toBe(before);
    expect(page.read().wire?.conversation.turns, 'the frame is not in the record').toHaveLength(1);
    expect(page.reads(), 'the page asked for a read on an update').toBe(asked);
    page.stop();
  });

  /**
   * **A settings pick moves the record the moment the core echoes it.** The
   * update carries the whole set and the seat folds it the way the terminal's
   * own arm folds it, so the panel follows the pick instead of waiting for a
   * read to carry the record past it.
   */
  it('moves the overrides a set landed, without asking for a read', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const asked = page.reads();

    page.land(
      updateOf({
        dictate_overrides: {
          key: LEAD,
          overrides: { styling: 'formal', structure: 'lists', context: null },
        },
      }),
    );

    expect(
      page.read().wire?.dictate_overrides,
      'the set the update carried never reached the seat',
    ).toEqual({ styling: 'formal', structure: 'lists', context: null });
    expect(page.reads(), 'the set was answered with a read rather than folded').toBe(asked);
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
   * **A page leaving a seat is the page leaving, not the seat ending.** The
   * record and the frames arriving after the reader has gone stay with the
   * seat, which is the whole change: a subscription owned by the page re-reads
   * the seat on the way back in, and one owned by the client draws what it
   * held.
   */
  it('keeps the seat after the last reader has gone', () => {
    const connection = drivable();
    const away = watch(connection);
    away.land(snapshotOf(LEAD));
    away.stop();
    const asked = connection.reads();

    connection.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));

    const back = watch(connection);
    expect(
      back.read().wire?.conversation.turns,
      'a frame that landed while nobody was showing the seat was lost',
    ).toHaveLength(1);
    expect(connection.reads(), 'the return asked the server for the seat again').toBe(asked);
    expect(connection.subscribes(), 'the return subscribed the seat a second time').toBe(1);
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
   * **A refusal is the one thing not worth holding.** The server watches
   * nothing for a seat it refused - "a refused subscribe leaves nothing to
   * hear" - so a cached refusal is a client holding a subscription that does
   * not exist, and a seat that starts later would never be reached again.
   */
  it('does not hold a seat the server refused', () => {
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
 * The slices no update carries - the process walk, the working tree, the pull
 * request, the monitors, the CLI's background tasks and the composer's three
 * lists - are the reason a page cannot simply follow the stream: nothing in it
 * mentions them. A slow read is what keeps them honest.
 *
 * The clock is faked here and nowhere else in this file, and the socket cases
 * above need the real one, so each case below starts and ends its own.
 */
describe('the slow read for what no update carries', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('asks for the session on the poll interval', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    const asked = page.reads();

    vi.advanceTimersByTime(POLL_MS + 1);

    expect(page.reads(), 'the page never asked again').toBe(asked + 1);
    page.stop();
  });

  it('stops asking once the last reader has gone', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    page.stop();
    const asked = page.reads();

    vi.advanceTimersByTime(POLL_MS * 3);

    expect(page.reads(), 'a stopped page is still reading the seat').toBe(asked);
  });

  /**
   * The poll runs for the seat being drawn, so a seat shown again arms it
   * again - and arms it without reading on the way in, which is the burst this
   * whole change exists to delete. A poll armed once at the seat's creation
   * and dropped with the first reader leaves every seat shown later drawing
   * the process walk and the working tree as they were when it was last read.
   */
  it('arms the poll again when the seat is shown again', () => {
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

    vi.advanceTimersByTime(POLL_MS + 1);

    expect(connection.reads(), 'the poll never came back for a seat shown again').toBe(asked + 1);
    back.stop();
  });

  it('takes the slices no update feeds from what the poll answered with', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, { work: { branch: 'main', changed: 3, gate: 'in_repo' } }));

    page.land(snapshotOf(LEAD, { work: { branch: 'feature', changed: 0, gate: 'in_repo' } }));

    expect(page.read().wire?.work.branch, 'the working tree never moved').toBe('feature');
    page.stop();
  });

  it('does not let an answer older than a seat swap stand in for the swap', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    expect(page.read().wire?.conversation.turns, 'precondition: the frame landed').toHaveLength(1);

    // A poll asks for the nine while the seat is still this occupant's.
    vi.advanceTimersByTime(POLL_MS + 1);
    const asked = page.reads();

    // The seat takes a new occupant before that answer comes back.
    page.land(updateOf(occupant('new')));
    expect(page.reads(), 'the swap asked while a read was already in flight').toBe(asked);

    // The poll's answer lands: the seat as it was BEFORE the swap.
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    expect(
      page.read().wire?.conversation.turns,
      'an answer from before the swap was published as the new record',
    ).toHaveLength(1);
    expect(page.reads(), 'the swap was never asked for a whole record').toBe(asked + 1);

    // And the answer the swap did ask for replaces the record rather than
    // merging into the one the previous occupant left.
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));
    expect(page.read().wire?.conversation.turns, 'the swap never took the record').toHaveLength(0);
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

  it('does not put a landing back from an answer older than the take', () => {
    const connection = drivable();
    const page = watch(connection);
    // The seat's record carries a landed take: the words the reader sent are
    // still its notice.
    page.land(snapshotOf(LEAD, { composer: { notice: landed() } }));

    // A frame that replaces the record asks for a whole one, which is what a
    // reconnect or a swap does.
    page.land(updateOf(occupant('new')));
    const asked = page.reads();

    // A take starts while that answer is in flight, which takes the notice off
    // the record the page holds.
    page.land(updateOf({ dictate_started: { key: LEAD, floor_db: -50, generation: 1 } }));

    // The answer lands: the seat as it was BEFORE the take, notice and all.
    // Adopted whole it puts the landing back, and the box takes words the
    // reader already sent a second time.
    page.land(snapshotOf(LEAD, { composer: { notice: landed() } }));

    expect(
      page.read().wire?.composer.notice,
      'an answer from before the take put the landing back',
    ).toBeNull();

    // **The whole record is still WANTED.** Merging keeps the frame-fed
    // composer, which is the fix above, but it also keeps the conversation and
    // the header this answer was asked for - so the want is kept and asked
    // again, the way a want raised while an ask was out is. Without this the
    // old occupant stands until a later REPLACES frame with a clean window or a
    // socket drop, which the five-second poll never does.
    expect(page.reads(), 'the whole record was not asked for again').toBe(asked + 1);

    // And the fresh ask converges: its clean answer is adopted whole.
    page.land(snapshotOf(LEAD, { composer: { notice: landed() }, work: { branch: 'new' } }));
    expect(page.read().wire?.composer.notice, 'the fresh answer was not taken').toEqual(landed());
    page.stop();

    // The control, and it is the guard's NARROWNESS it holds: the same answer
    // with nothing outrunning it is adopted whole and does NOT ask again. An
    // over-broad guard re-asks here and fails this.
    const control = drivable();
    const other = watch(control);
    other.land(snapshotOf(LEAD, { composer: { notice: landed() } }));
    other.land(updateOf(occupant('new')));
    const askedOnce = other.reads();
    other.land(snapshotOf(LEAD, { composer: { notice: landed() } }));

    expect(other.read().wire?.composer.notice, 'the control took the record').toEqual(landed());
    expect(other.reads(), 'a clean answer was asked for again').toBe(askedOnce);

    // And the narrowness that keeps the re-ask from being over-broad: a POLL's
    // answer outrun by a frame is a merge already, and its want was nothing, so
    // it asks for nothing more.
    const polling = drivable();
    const timed = watch(polling);
    timed.land(snapshotOf(LEAD));
    vi.advanceTimersByTime(POLL_MS + 1);
    const polled = timed.reads();
    timed.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    timed.land(snapshotOf(LEAD));

    expect(timed.reads(), "a poll's outrun answer asked again").toBe(polled);
    timed.stop();
  });

  it('does not walk back a slice an update already advanced', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));
    const advanced = page.read().wire;

    // A poll's answer, taken before the frame landed: its conversation is
    // empty, and taking it would drop the frame the page is holding.
    page.land(snapshotOf(LEAD, { conversation: { turns: [], compaction_count: 0 } }));

    expect(page.read().wire?.conversation, 'the poll walked the conversation back').toEqual(
      advanced?.conversation,
    );
    page.stop();
  });
});

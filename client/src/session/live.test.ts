// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { createRawSnippet, flushSync, mount, unmount } from 'svelte';
import type { AddressInfo, RawData, WebSocketServer as Server } from 'ws';
import { afterEach, describe, expect, it } from 'vitest';

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
import type { Store, StoreValue } from '../stores';
import type { SessionSlot } from '../wire/types';
import { watchSession, type SessionRead } from './live';
import Session from './Session.svelte';

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

describe('the session page over a socket', () => {
  it('draws the seat record the server answered with', async () => {
    await open(sessionFixture);

    expect(sections(), 'the seat record never reached the page').toContain('git');
    const facts = document.querySelector('.sess .facts');
    expect(facts?.textContent, 'the header drew no facts from the record').toContain('max');
  });

  /**
   * The rail comes from the home subject, which the shell holds for its whole
   * life - so this is also the assertion that the page reads two subjects and
   * not one.
   */
  it('draws the rail from the home the connection also carries', async () => {
    await open(sessionFixture);
    expect(drawn()).toContain('class="pj cur"');
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
  status(): ConnectionStatus;
}

function drivable(): Driveable {
  const listeners = new Set<(message: ServerMessage) => void>();
  const watchers = new Set<(status: ConnectionStatus) => void>();
  const snapshot = new Map<string, unknown>();
  let reads = 0;

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
      state: () => ({ kind: 'ready' }),
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
      snapshot.set(subjectKey(what), null);
      return store(what);
    },
    unsubscribe: () => {},
    refresh: () => {
      reads += 1;
    },
    dispatch: () => {
      throw new Error('this page dispatched a command the case did not expect');
    },
    more: () => false,
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

  it('stops listening and lets the subscription go with the last reader', () => {
    const connection = drivable();
    const page = watch(connection);
    page.land(snapshotOf(LEAD));
    page.stop();

    const held = page.read().wire;
    page.land(updateOf({ chat_appended: { key: LEAD, msg: spoke('hello') } }));

    expect(page.read().wire, 'a stopped page is still following the seat').toBe(held);
  });
});

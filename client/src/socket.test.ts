// Bare, because this file runs in the node environment: under jsdom the same
// import lands on ws's browser shim, which throws, which is why the shell's
// test requires it through node instead. Do not set this file's environment
// to jsdom, and do not write the directive that would: that breaks this line.
import { type AddressInfo, type RawData, WebSocketServer } from 'ws';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { connect, type Connection } from './socket';
import {
  MIN_PROTOCOL,
  PROTOCOL_VERSION,
  slotOf,
  type ClientMessage,
  type ServerMessage,
  type Subject,
} from './protocol';
import { DEFAULT_AXES } from './session/wire';
import { DEFAULT_SETTINGS, type SessionSlot } from './wire/types';

/** A forge that speaks the protocol and nothing else. */
async function stubServer() {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;

  const sockets = new Set<import('ws').WebSocket>();
  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });

  /** Everything the client has sent, in order. */
  const received: ClientMessage[] = [];
  /** Every binary frame the client sent, in order - a browser image's bytes. */
  const binary: Buffer[] = [];
  /**
   * Both kinds in ONE log, in arrival order.
   *
   * The order is the contract for a browser answer - the answer first, then
   * its image frames - and two separate arrays cannot state it: a frame that
   * overtook its answer would look exactly like one that followed it.
   */
  const arrivals: ({ kind: 'json'; message: ClientMessage } | { kind: 'frame'; bytes: Buffer })[] =
    [];
  server.on('connection', (socket) => {
    socket.on('message', (data: RawData, isBinary: boolean) => {
      if (isBinary) {
        const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data as ArrayBuffer);
        binary.push(bytes);
        arrivals.push({ kind: 'frame', bytes });
        return;
      }
      const message = JSON.parse(frame(data)) as ClientMessage;
      received.push(message);
      arrivals.push({ kind: 'json', message });
    });
  });

  return {
    url: `ws://127.0.0.1:${port}/socket`,
    received,
    binary,
    arrivals,
    /** Say something to every client attached. */
    send(message: ServerMessage) {
      for (const socket of sockets) socket.send(JSON.stringify(message));
    },
    /** One frame exactly as given, for a frame that is not JSON at all. */
    raw(text: string) {
      for (const socket of sockets) socket.send(text);
    },
    /** Drop every client, as a server restart or a lost network would. */
    drop() {
      for (const socket of sockets) socket.terminate();
    },
    async close() {
      for (const socket of sockets) socket.terminate();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/** One frame as text. Everything here is JSON, which `ws` hands over as a Buffer. */
function frame(data: RawData): string {
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
  return data.toString('utf8');
}

/** The commands among what the server received, which is where `reply_to` is decided. */
function commands(received: ClientMessage[]) {
  return received.filter(
    (message): message is Extract<ClientMessage, { kind: 'command' }> => message.kind === 'command',
  );
}

/** Wait until `check` holds, or fail rather than hang. */
async function until(check: () => boolean, what: string) {
  for (let i = 0; i < 100; i += 1) {
    if (check()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${what}`);
}

/** Give anything already in flight a chance to arrive, for an absence to assert. */
async function settle() {
  await new Promise((resolve) => setTimeout(resolve, 80));
}

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

const opened: Connection[] = [];
const servers: Awaited<ReturnType<typeof stubServer>>[] = [];

afterEach(async () => {
  vi.restoreAllMocks();
  for (const conn of opened.splice(0)) conn.close();
  for (const server of servers.splice(0)) await server.close();
});

async function connected() {
  const server = await stubServer();
  servers.push(server);
  const conn = connect(server.url);
  opened.push(conn);
  await until(() => conn.status() === 'open', 'the socket to open');
  return { server, conn };
}

describe('the connection', () => {
  /**
   * A store holds the snapshot it was answered with AND the updates that
   * followed, because a view redraws from either: the snapshot is the whole
   * subject and an update is one change to it. Both updates, in order - a
   * store that replaced would hold one and read as complete.
   */
  it('holds a snapshot per subject and appends the updates that follow', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the subscribe to arrive');

    server.send({ kind: 'snapshot', subject: HOME, data: { projects: [] } });
    // Unit variants cross as their name alone, not as a `kind` field.
    server.send({ kind: 'update', update: 'catalog_loaded' });
    server.send({ kind: 'update', update: 'cli_version_changed' });

    const store = conn.store(HOME);
    await until(() => (store?.updates().length ?? 0) === 2, 'both updates');
    expect(store?.snapshot()).toEqual({ projects: [] });
    expect(store?.updates()).toEqual(['catalog_loaded', 'cli_version_changed']);
    expect(store?.state()).toEqual({ kind: 'ready' });
  });

  /**
   * The declaration decides whether the core parks a turn on this client's
   * reply. Sending it as `false` on a client that draws a dock would make
   * every prompt it shows resolve `Cancelled` - the turn fails rather than
   * waiting for the person looking at it.
   */
  it('declares whether it can answer, on the first subscribe', async () => {
    const { server, conn } = await connected();

    conn.subscribe(HOME, { answering: true });
    await until(() => server.received.length === 1, 'the subscribe to arrive');
    expect(server.received[0]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: true,
      browser: false,
    });

    // Off unless said otherwise, so a read-only client cannot hang a turn.
    conn.subscribe('usage');
    await until(() => server.received.length === 2, 'the second subscribe');
    expect(server.received[1]).toEqual({
      kind: 'subscribe',
      what: 'usage',
      answering: false,
      browser: false,
    });
  });

  /**
   * The app's first move: `connect` returns before the socket is up, so a
   * subscribe made in that window reached no server and has to be replayed
   * when one answers. Recording it only once open would drop it entirely.
   */
  it('asks for a subject subscribed before the socket opened', async () => {
    const server = await stubServer();
    servers.push(server);
    const conn = connect(server.url);
    opened.push(conn);
    expect(conn.status()).toBe('connecting');

    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the subscribe to arrive');
    expect(server.received[0]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: false,
      browser: false,
    });
  });

  /** The greeting is what configures the client before it draws anything. */
  it('keeps the settings the greeting carried', async () => {
    const { server, conn } = await connected();

    server.send({
      kind: 'greeting',
      version: PROTOCOL_VERSION,
      settings: { mark: 'klin', theme: null, font: null, dictate: DEFAULT_AXES },
    });
    await until(() => conn.settings() !== null, 'the greeting to land');
    expect(conn.settings()).toEqual({
      mark: 'klin',
      theme: null,
      font: null,
      dictate: DEFAULT_AXES,
    });
  });

  /**
   * The protocol's only mismatch detector, and it is checked on EVERY
   * greeting rather than the first: a page left open across a forge upgrade
   * reconnects to a protocol it cannot read, and drawing against that shape
   * silently is the failure the check exists to prevent. It does not retry,
   * because a retry meets the same answer.
   */
  it('stops when a later greeting speaks another protocol', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the subscribe');

    server.send({
      kind: 'greeting',
      version: PROTOCOL_VERSION + 1,
      settings: DEFAULT_SETTINGS,
    });

    await until(() => conn.status() === 'mismatched', 'the mismatch to be reported');
  });

  /**
   * The floor: a server one step back is read rather than refused, because
   * a client with no way to draw is worse than one drawing against a shape
   * whose read is proven - and refusing one is what leaves a person with no
   * channel to their own forge at all.
   */
  it('keeps reading a server one step back, and says so', async () => {
    const { server, conn } = await connected();

    server.send({ kind: 'greeting', version: MIN_PROTOCOL, settings: DEFAULT_SETTINGS });

    await until(() => conn.skew() !== null, 'the skew to be recorded');
    expect(conn.status(), 'a server in range was refused').toBe('open');
    expect(conn.skew()).toEqual({ serverProtocol: MIN_PROTOCOL, serverVersion: null });

    // And the socket is genuinely still working, not merely not closed.
    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the subscribe to reach the server');
  });

  /**
   * A server below the floor is refused, and the refusal has to name the two
   * halves and the command: the numbers alone name no build and no way out.
   */
  it('refuses a server below the floor, naming the halves and the command', async () => {
    const warned = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { server, conn } = await connected();

    server.send({ kind: 'greeting', version: MIN_PROTOCOL - 1, settings: DEFAULT_SETTINGS });

    await until(() => conn.status() === 'mismatched', 'the refusal');
    const said = warned.mock.calls.flat().join(' ');
    expect(said).toContain(`protocol ${MIN_PROTOCOL - 1}`);
    expect(said).toContain(`protocol ${PROTOCOL_VERSION}`);
    expect(said, 'the refusal named no way out').toContain('just install');
    // And it does not retry into the same answer.
    await settle();
    expect(conn.status()).toBe('mismatched');
  });

  /**
   * The release identity the greeting carries is what lets a skew name a
   * build rather than a number, so the socket hands it on rather than
   * keeping it to itself - including on a refusal, which is the case with a
   * release to name today.
   */
  it('carries the release a refused greeting named', async () => {
    const warned = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { server, conn } = await connected();

    server.send({
      kind: 'greeting',
      version: PROTOCOL_VERSION + 1,
      forge_version: '1.0.116+abc1234',
      forge_version_short: '1.0.116+abc1234',
      settings: DEFAULT_SETTINGS,
    });

    await until(() => conn.status() === 'mismatched', 'the refusal');
    expect(conn.skew(), 'the refusal kept the build it was named').toEqual({
      serverProtocol: PROTOCOL_VERSION + 1,
      serverVersion: '1.0.116+abc1234',
    });
    expect(warned.mock.calls.flat().join(' ')).toContain('1.0.116+abc1234');
  });

  /** This client's own protocol is not a skew, and says nothing. */
  it('shows no skew for a greeting that agrees', async () => {
    const { server, conn } = await connected();

    server.send({ kind: 'greeting', version: PROTOCOL_VERSION, settings: DEFAULT_SETTINGS });

    await until(() => conn.settings() !== null, 'the greeting to land');
    expect(conn.skew()).toBeNull();
  });

  /**
   * The skew is what the LAST greeting said rather than a latch: a forge
   * swapped underneath a running client reconnects to one that agrees, and
   * a notice left standing would accuse a build that is current.
   */
  it('clears a tolerated skew when a later greeting agrees', async () => {
    const { server, conn } = await connected();

    server.send({ kind: 'greeting', version: MIN_PROTOCOL, settings: DEFAULT_SETTINGS });
    await until(() => conn.skew() !== null, 'the skew to be recorded');

    server.send({ kind: 'greeting', version: PROTOCOL_VERSION, settings: DEFAULT_SETTINGS });

    await until(() => conn.skew() === null, 'the skew to clear');
    expect(conn.status(), 'the live connection was disturbed by a later greeting').toBe('open');
  });

  /**
   * A command is the core's own enum, passed through rather than rebuilt:
   * the variant's name around its own fields, which is what the core's
   * externally-tagged enum decodes and what a `kind` field is not.
   *
   * Which of the two envelope shapes it takes is the client's to decide, not
   * the caller's, because the server refuses both mismatches - and it refuses
   * them before dispatching, so getting this wrong is a command that silently
   * never runs rather than one that reports an error. All four of the names
   * are dispatched, because a name that fell out of the set would show up as
   * a command sent with no reply channel at all.
   */
  it('sets reply_to from the command, and only for the four that answer', async () => {
    const { server, conn } = await connected();

    const plain = [
      conn.dispatch({ cancel: { key: LEAD } }),
      conn.dispatch({ set_mode: { key: LEAD, mode: 'default' } }),
    ];
    const asks = [
      conn.dispatch({ spawn_worker: { project_key: 'proj', label: 'w1' } }),
      conn.dispatch({ despawn_worker: { project_key: 'proj', label: 'w1', force: false } }),
      conn.dispatch({ upsert_review_thread: { project: 'proj', branch: 'main' } }),
      conn.dispatch({ submit_review: { project: 'proj', branch: 'main' } }),
    ];
    await until(() => commands(server.received).length === 6, 'all six commands');

    const sent = commands(server.received);
    expect(plain).toEqual([null, null]);
    expect(sent.slice(0, 2).map((command) => command.reply_to)).toEqual([null, null]);

    const ids = sent.slice(2).map((command) => command.reply_to);
    for (const id of ids) expect(typeof id).toBe('number');
    // Four asks, four channels: a shared id hands a caller another's answer.
    expect(new Set(ids).size).toBe(4);

    // Nothing here answers them, so they are settled rather than left to
    // reject unhandled when the connection closes.
    conn.close();
    await Promise.allSettled(asks.filter((ask) => ask !== null));
  });

  /** A command the server never hears is a command that never ran, and nothing else says so. */
  it('refuses a command and a page-ask rather than send them nowhere', async () => {
    const { conn } = await connected();
    conn.close();

    expect(() => conn.dispatch({ cancel: { key: LEAD } })).toThrow('the socket is not open');
    expect(() => conn.dispatch({ despawn_worker: { project_key: 'p', label: 'w' } })).toThrow(
      'the socket is not open',
    );
    expect(conn.more(LEAD), 'a page-ask that went nowhere read as one that went').toBe(false);
  });

  it('asks for more turns of one conversation', async () => {
    const { server, conn } = await connected();

    expect(conn.more(LEAD, 'cursor-1', 20)).toBe(true);
    conn.more(LEAD);
    await until(() => server.received.length === 2, 'both asks');
    expect(server.received[0]).toEqual({
      kind: 'more',
      conversation: LEAD,
      before: 'cursor-1',
      turns: 20,
    });
    expect(server.received[1]).toEqual({
      kind: 'more',
      conversation: LEAD,
      before: null,
      turns: 20,
    });
  });

  /**
   * A `reply` carries the answer to a command that asked for one, and a
   * refusal arrives as a reply rather than as an error - so a client
   * awaiting one is never left watching a channel that stays empty.
   */
  it('hands a reply to the caller that asked, by its own id', async () => {
    const { server, conn } = await connected();

    const answer = conn.dispatch({ despawn_worker: { project_key: 'proj', label: 'w1' } });
    await until(() => commands(server.received).length === 1, 'the command to arrive');
    const [sent] = commands(server.received);
    // The reply is answered on the id the command itself asked to hear it on.
    if (answer === null || sent === undefined || sent.reply_to === null) {
      throw new Error('a despawn is one of the four that answer through a reply');
    }

    // A refusal is the server's own words in the body, which for this answer
    // is a bare string rather than an object.
    server.send({ kind: 'reply', reply_to: sent.reply_to, body: 'no such worker' });

    await expect(answer).resolves.toBe('no such worker');
  });

  /**
   * A command in flight when the socket goes has no answer coming, and a
   * promise that never settles is a button that never returns. Resolving
   * instead of rejecting would read to the caller as a successful spawn.
   */
  it('rejects what a command was holding when the socket drops', async () => {
    const { server, conn } = await connected();
    const answer = conn.dispatch({ spawn_worker: { project_key: 'proj', label: 'w1' } });
    await until(() => commands(server.received).length === 1, 'the command to arrive');

    server.drop();

    await expect(answer).rejects.toThrow('the socket dropped before answering');
  });

  it('rejects what a command was holding when the connection closes', async () => {
    const { server, conn } = await connected();
    const answer = conn.dispatch({ spawn_worker: { project_key: 'proj', label: 'w1' } });
    await until(() => commands(server.received).length === 1, 'the command to arrive');

    conn.close();

    await expect(answer).rejects.toThrow('the connection was closed');
  });

  /**
   * The socket carries no history, so a reconnect re-asks. The store keeps
   * what it had until the fresh snapshot arrives, or the reader watches the
   * page empty and refill.
   */
  it('re-subscribes after the socket drops without blanking the page', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the first subscribe');
    server.send({ kind: 'snapshot', subject: HOME, data: { projects: ['a'] } });
    await until(() => conn.store(HOME)?.snapshot() !== null, 'the first snapshot');

    server.drop();
    await until(() => server.received.length >= 2, 'the re-subscribe');
    // The re-ask carries the same subject and the same declaration.
    expect(server.received[1]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: false,
      browser: false,
    });
    // And the store is still holding what it had while the server answers.
    expect(conn.store(HOME)?.snapshot()).toEqual({ projects: ['a'] });

    server.send({ kind: 'snapshot', subject: HOME, data: { projects: ['a', 'b'] } });
    await until(
      () =>
        JSON.stringify(conn.store(HOME)?.snapshot()) === JSON.stringify({ projects: ['a', 'b'] }),
      'the fresh snapshot',
    );
  });

  /**
   * A reconnect re-asks as many times as the subject was subscribed, and with
   * the declaration the connection holds.
   *
   * One unsubscribe drops one of the server's entries, so a subject held
   * twice that was re-asked once leaves the two sides out of step: the next
   * unsubscribe takes it away from a caller still drawing it. And a re-ask
   * that dropped `answering` to the default would turn a client that draws a
   * dock into a read-only observer after every blip.
   */
  it('re-asks once per subscription, carrying the declaration it made', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME, { answering: true });
    conn.subscribe(HOME);
    conn.unsubscribe(HOME);
    await until(() => server.received.length === 3, 'both subscribes and the unsubscribe');

    server.drop();
    await until(() => server.received.length >= 4, 'the re-ask');
    expect(server.received[3]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: true,
      browser: false,
    });

    // One entry is left, so exactly one re-ask: a second would mean the count
    // came back as two.
    await settle();
    expect(server.received.slice(4)).toEqual([]);
  });

  /**
   * Two subscriptions to one subject are two re-asks, which is the count the
   * server holds: one unsubscribe drops one of its entries, so a reconnect
   * that re-asked once for a subject held twice would leave the server's
   * count lower than this side's, and the next unsubscribe would take the
   * subject away from a caller still drawing it.
   */
  it('re-asks once per subscription, not once per subject', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME);
    conn.subscribe(HOME);
    await until(() => server.received.length === 2, 'both subscribes');

    server.drop();
    await until(() => server.received.length >= 4, 'both re-asks');

    await settle();
    expect(server.received.length, 'a subject held twice was re-asked for once').toBe(4);
  });

  /**
   * A refusal names no subject, so it belongs to the oldest ask still waiting
   * on an answer - the order the server answers in. Taking the newest instead
   * would refuse the subscription the server just accepted, and the refused
   * one would read as loading for ever.
   */
  it('gives a refusal to the oldest subscribe still waiting', async () => {
    const { server, conn } = await connected();
    const lead: Subject = { session: LEAD };
    const w1: Subject = { session: { org: 'TestOrg', project: 'proj', label: 'w1' } };
    const first = conn.subscribe(lead);
    const second = conn.subscribe(w1);
    await until(() => server.received.length === 2, 'both subscribes');

    server.send({ kind: 'error', what: 'subscribe', why: 'forge holds no session for that seat' });

    await until(() => first.state().kind === 'refused', 'the refusal to land');
    expect(first.state()).toEqual({ kind: 'refused', why: 'forge holds no session for that seat' });
    expect(second.state(), 'the refusal went to the newer subscription').toEqual({
      kind: 'loading',
    });
  });

  /**
   * A refresh over a store that is already ready is refused with no store in
   * `loading` to find, so attribution is a queue of asks rather than a scan
   * of states. Without it the store keeps a `ready` state and a full
   * snapshot while the server has dropped the subscription, and nothing for
   * that subject ever arrives again with nothing saying so.
   */
  it('refuses a store that was already answered and then re-asked', async () => {
    const { server, conn } = await connected();
    const seat: Subject = { session: LEAD };
    const store = conn.subscribe(seat);
    await until(() => server.received.length === 1, 'the subscribe');
    server.send({ kind: 'snapshot', subject: seat, data: { who: 'lead' } });
    await until(() => store.state().kind === 'ready', 'the snapshot');

    conn.refresh(seat);
    await until(() => server.received.length === 3, 'the refresh pair');
    server.send({ kind: 'error', what: 'subscribe', why: 'forge holds no session for that seat' });

    await until(() => store.state().kind === 'refused', 'the refusal to land');
  });

  /**
   * Every ask outstanding when the socket dropped died with it, and the
   * reconnect asks again. Leaving the dead ones queued puts stale keys in
   * front of the live ones, and a refusal pops the oldest - so it lands on a
   * store the server had already answered while the genuinely refused one
   * reads as loading for the life of the connection.
   */
  it('forgets the asks a drop killed, so a later refusal finds its own', async () => {
    const { server, conn } = await connected();
    const lead: Subject = { session: LEAD };
    const w1: Subject = { session: { org: 'TestOrg', project: 'proj', label: 'w1' } };
    const w2: Subject = { session: { org: 'TestOrg', project: 'proj', label: 'w2' } };

    const first = conn.subscribe(lead);
    const second = conn.subscribe(w1);
    await until(() => server.received.length === 2, 'both subscribes');
    server.send({ kind: 'snapshot', subject: lead, data: { who: 'lead' } });
    await until(() => first.state().kind === 'ready', 'lead to be answered');

    // `w1` is still outstanding when the socket goes.
    server.drop();
    await until(() => server.received.length >= 4, 'both re-asks');
    server.send({ kind: 'snapshot', subject: lead, data: { who: 'lead' } });
    server.send({ kind: 'snapshot', subject: w1, data: { who: 'w1' } });
    await until(() => second.state().kind === 'ready', 'w1 to be answered');

    const third = conn.subscribe(w2);
    await until(() => server.received.length >= 5, 'the third subscribe');
    server.send({ kind: 'error', what: 'subscribe', why: 'forge holds no session for that seat' });

    await until(() => third.state().kind === 'refused', 'the refusal to land on w2');
    expect(second.state(), 'the refusal went to a subscription already answered').toEqual({
      kind: 'ready',
    });
  });

  /**
   * One unsubscribe drops one entry: a second subscribe to one subject adds
   * a second entry rather than replacing the first, because two
   * subscriptions to one seat are one seat still being shown.
   */
  it('drops one subscription per unsubscribe', async () => {
    const { server, conn } = await connected();

    conn.subscribe(HOME);
    conn.subscribe(HOME);
    conn.unsubscribe(HOME);
    await until(() => server.received.length === 3, 'all three messages');
    expect(server.received[2]).toEqual({ kind: 'unsubscribe', what: HOME });
    // Still held: one of the two is still open.
    expect(conn.store(HOME)).toBeDefined();

    conn.unsubscribe(HOME);
    await until(() => server.received.length === 4, 'the second unsubscribe');
    expect(conn.store(HOME)).toBeUndefined();
  });

  /** A seat is addressed by its slot, so two seats are two stores. */
  it('keeps one store per seat, keyed by the slot', async () => {
    const { server, conn } = await connected();
    const lead: Subject = { session: LEAD };
    const w1: Subject = { session: { org: 'TestOrg', project: 'proj', label: 'w1' } };

    conn.subscribe(lead);
    conn.subscribe(w1);
    await until(() => server.received.length === 2, 'both subscribes');
    server.send({ kind: 'snapshot', subject: lead, data: { who: 'lead' } });
    server.send({ kind: 'snapshot', subject: w1, data: { who: 'w1' } });

    await until(() => conn.store(w1)?.snapshot() !== null, 'the second snapshot');
    expect(conn.store(lead)?.snapshot()).toEqual({ who: 'lead' });
    expect(conn.store(w1)?.snapshot()).toEqual({ who: 'w1' });
  });

  /**
   * An update carrying a seat goes to that seat's store, which is what makes
   * a subscription a seat: routing everything to the home store would leave a
   * session page's store holding its snapshot and never a word after it.
   *
   * And the home gets it only when the fleet classification says so. A turn's
   * own words are the bulk of the stream and no row shows one, so a home
   * store fed every frame a watched seat emitted would hold the conversation
   * the server deliberately withholds from a home subscriber.
   */
  it('routes an update to its seat, and to the home only when the fleet cares', async () => {
    const { server, conn } = await connected();
    const lead: Subject = { session: LEAD };
    conn.subscribe(lead);
    conn.subscribe(HOME);
    await until(() => server.received.length === 2, 'both subscribes');

    server.send({ kind: 'update', update: { turn_cancelled: { key: LEAD } } });
    const seat = conn.store(lead);
    await until(() => (seat?.updates().length ?? 0) === 1, 'the seat to hear it');
    expect(conn.store(HOME)?.updates()).toEqual([{ turn_cancelled: { key: LEAD } }]);

    server.send({
      kind: 'update',
      update: { chat_appended: { key: LEAD, msg: { type: 'assistant' } } },
    });
    await until(() => (seat?.updates().length ?? 0) === 2, 'the second update');
    expect(conn.store(HOME)?.updates(), "a turn's own words reached the home store").toHaveLength(
      1,
    );
  });

  /**
   * A refused subscribe is an answer rather than a silence - the server sends
   * one for a seat nobody has started, so a page can say why instead of
   * drawing an empty snapshot as a broken page. `snapshot: null` cannot carry
   * that, so the store carries the refusal itself.
   */
  it('gives a refused subscribe a store that says so', async () => {
    const { server, conn } = await connected();
    const seat: Subject = { session: LEAD };
    const store = conn.subscribe(seat);
    await until(() => server.received.length === 1, 'the subscribe to arrive');

    server.send({ kind: 'error', what: 'subscribe', why: 'forge holds no session for that seat' });

    await until(() => store.state().kind === 'refused', 'the refusal to land');
    expect(store.state()).toEqual({
      kind: 'refused',
      why: 'forge holds no session for that seat',
    });
    expect(store.snapshot()).toBeNull();
  });

  /** Nothing replays a subscribe made after the socket went, so a live store would be a lie. */
  it('refuses a subscribe on a closed connection', async () => {
    const { conn } = await connected();
    conn.close();

    const store = conn.subscribe(HOME);
    expect(store.state()).toEqual({ kind: 'refused', why: 'this connection is closed' });
  });

  /**
   * A page keeps its contents across a drop, which is right, and has to be
   * able to say it is reconnecting, which is the other half: without it a
   * page draws pre-drop data as though it were live.
   */
  it('says where the connection is, and when it drops', async () => {
    const { server, conn } = await connected();
    const seen: string[] = [];
    conn.onStatus((next) => seen.push(next));

    server.drop();
    await until(() => seen.includes('connecting'), 'the drop to be reported');
    expect(conn.status()).toBe('connecting');

    conn.close();
    expect(conn.status()).toBe('closed');
  });

  /**
   * The core's own `slot()`, read off the wire: the 41 variants carrying a
   * `key: SessionSlot` have a seat, and the rest have none at all.
   */
  it('reads a seat off the update that carries one, and none off the rest', () => {
    expect(slotOf({ turn_cancelled: { key: LEAD } })).toEqual(LEAD);
    expect(slotOf('catalog_loaded')).toBeNull();
    expect(slotOf({ service_status: { state: 'ok' } })).toBeNull();
  });

  /** An error names what failed and why, and reaches a listener rather than being dropped. */
  it('reports an error the core sent', async () => {
    const { server, conn } = await connected();
    const seen: ServerMessage[] = [];
    conn.onMessage((message) => seen.push(message));

    server.send({ kind: 'error', what: 'dispatch', why: 'that seat is gone' });
    await until(() => seen.length === 1, 'the error to arrive');
    expect(seen[0]).toEqual({ kind: 'error', what: 'dispatch', why: 'that seat is gone' });
  });

  /**
   * A frame that is not JSON is reported and dropped rather than thrown, and
   * the frame after it still arrives. This is the one boundary the client's
   * whole surface is narrowed at.
   *
   * The second half is not what the guard buys: `ws` swallows a handler's
   * throw and emits `error` instead, so deleting the try/catch leaves the next
   * frame arriving anyway and only the report below fails. It is here for the
   * change that makes a decode failure fatal, by closing the connection or by
   * rethrowing, and that is the shape it catches.
   */
  it('reports a frame that does not parse and keeps reading the ones after it', async () => {
    const warned = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { server, conn } = await connected();
    const seen: ServerMessage[] = [];
    conn.onMessage((message) => seen.push(message));

    server.raw('not a frame at all');
    server.send({ kind: 'update', update: 'catalog_loaded' });

    await until(() => seen.length === 1, 'the frame after the unreadable one');
    expect(seen[0], 'the frame after an unreadable one did not arrive').toEqual({
      kind: 'update',
      update: 'catalog_loaded',
    });
    expect(
      warned.mock.calls.flat().join(' '),
      'a frame that does not parse was dropped without a word',
    ).toContain('a frame this client could not read');
  });

  /**
   * One page's listener throwing must not silence the pages after it: a throw
   * out of the loop leaves every listener behind it with the last frame it had
   * and nothing saying why.
   */
  it('gives a frame to every listener when one of them throws', async () => {
    const warned = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { server, conn } = await connected();
    const threw: ServerMessage[] = [];
    const behind: ServerMessage[] = [];
    conn.onMessage((message) => {
      threw.push(message);
      throw new Error('this page is broken');
    });
    conn.onMessage((message) => behind.push(message));

    server.send({ kind: 'update', update: 'catalog_loaded' });

    // The throwing listener is the one that runs first, so it hears the frame
    // in either case; waiting on it is waiting for the delivery, not for the
    // outcome.
    await until(() => threw.length === 1, 'the frame to be delivered');
    await settle();
    expect(threw).toHaveLength(1);
    expect(behind, 'a listener that threw silenced the listener behind it').toEqual([
      { kind: 'update', update: 'catalog_loaded' },
    ]);
    expect(
      warned.mock.calls.flat().join(' '),
      'a listener that threw was silenced without a word',
    ).toContain('a message listener threw');
  });
});

describe('the browser role', () => {
  /**
   * The capability is declared on the subscribe and kept across a reconnect:
   * a drop that re-asked for the subject without it would leave the client
   * attached, drawing, and unable to host anything the session asked for.
   */
  it('declares the capability, and re-declares it when the socket comes back', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME, { browser: true });
    await until(() => server.received.length === 1, 'the subscribe to arrive');
    expect(server.received[0]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: false,
      browser: true,
    });

    server.drop();
    await until(() => server.received.length >= 2, 'the re-ask');
    expect(server.received[1]).toEqual({
      kind: 'subscribe',
      what: HOME,
      answering: false,
      browser: true,
    });
  });

  /**
   * One ask in, one answer out, under the ask's own id: the parts the handler
   * returned cross as the answer, and a failure crosses as the reason rather
   * than as an empty result.
   */
  it('answers an ask with the parts its handler returned', async () => {
    const { server, conn } = await connected();
    conn.onBrowserAsk(async (ask) => {
      // A promise, because the shell's own handler is one: it goes to the
      // Tauri side and comes back.
      await Promise.resolve();
      return {
        parts: [
          { type: 'text', text: `${ask.tool} -> ${String((ask.args as { url: string }).url)}` },
        ],
      };
    });

    server.send({
      kind: 'browser_ask',
      id: 7,
      seat: LEAD,
      tool: 'browser_navigate',
      args: { url: 'https://example.com' },
    });
    await until(() => server.received.length === 1, 'the answer to arrive');
    expect(server.received[0]).toEqual({
      kind: 'browser_answer',
      id: 7,
      parts: [{ type: 'text', text: 'browser_navigate -> https://example.com' }],
      error: null,
    });
  });

  /**
   * An image part's mime crosses on the answer and its BYTES ride a binary
   * frame under the same id: the tag, the id big-endian, then the bytes.
   */
  it('sends an image part as a mime on the answer and bytes in their own frame', async () => {
    const { server, conn } = await connected();
    conn.onBrowserAsk(() => ({
      parts: [
        { type: 'text', text: 'captured' },
        { type: 'image', mime_type: 'image/png', bytes: new Uint8Array([0x89, 0x50]) },
      ],
    }));

    server.send({
      kind: 'browser_ask',
      id: 9,
      seat: LEAD,
      tool: 'browser_take_screenshot',
      args: {},
    });
    await until(() => server.received.length === 1 && server.binary.length === 1, 'the answer');

    expect(server.received[0]).toEqual({
      kind: 'browser_answer',
      id: 9,
      parts: [
        { type: 'text', text: 'captured' },
        { type: 'image', mime_type: 'image/png' },
      ],
      error: null,
    });
    const frame = server.binary[0];
    expect(frame?.[0], 'the browser-image kind tag').toBe(1);
    expect(frame?.readBigUInt64BE(1), 'the answer id, big-endian').toBe(9n);
    expect([...(frame?.subarray(9) ?? [])], 'the image bytes').toEqual([0x89, 0x50]);

    // **The answer goes FIRST.** A frame that overtook the answer declaring
    // its image is a pair the server refuses - and fails the call over - so
    // the order is asserted as it arrived, rather than from two separate
    // lists that cannot tell an overtaking frame from a following one.
    expect(
      server.arrivals.map((arrival) => arrival.kind),
      'the answer crossed before the frame it belongs to',
    ).toEqual(['json', 'frame']);
  });

  /**
   * **A frame that could not be sent cannot be skipped.** The answer is
   * already on the wire, so its image can never arrive and the tool call
   * would wait on a promise this client cannot keep. The connection drops
   * instead: the server fails that ask naming the host that went away, and
   * the reconnect re-declares the capability.
   */
  it('drops the connection when an image frame cannot be sent', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME, { browser: true });
    await until(() => server.received.length === 1, 'the subscribe to arrive');
    conn.onBrowserAsk(() => ({
      parts: [{ type: 'image', mime_type: 'image/png', bytes: new Uint8Array([1]) }],
    }));

    // The fault is a FRAME that cannot be sent, not the answer: the answer is
    // a string and goes through, and the frame after it is what fails.
    const send = vi.spyOn(WebSocket.prototype, 'send').mockImplementation((data: unknown) => {
      if (typeof data !== 'string') throw new Error('the socket is gone');
    });
    server.send({
      kind: 'browser_ask',
      id: 3,
      seat: LEAD,
      tool: 'browser_take_screenshot',
      args: {},
    });
    await until(
      () => conn.status() !== 'open',
      'the connection to drop rather than leave the answer half-sent',
    );
    send.mockRestore();
  });

  /**
   * A handler that throws is a failed tool call with the reason named, and a
   * connection that cannot host answers the same shape: the session's turn
   * gets an answer either way rather than waiting on one that never comes.
   */
  it('answers a failure rather than nothing when it cannot serve the ask', async () => {
    const { server, conn } = await connected();
    conn.onBrowserAsk(() => {
      throw new Error('the driver is not running');
    });
    server.send({ kind: 'browser_ask', id: 1, seat: LEAD, tool: 'browser_close', args: {} });
    await until(() => server.received.length === 1, 'the failed answer');
    const failed = server.received[0];
    expect(failed).toMatchObject({ kind: 'browser_answer', id: 1, parts: [] });
    const why = server.received.find((m) => m.kind === 'browser_answer')?.error ?? '';
    expect(why).toContain('the driver is not running');

    // No handler at all is the same shape: a client that was sent an ask it
    // has no way to serve says so.
    const alone = await connected();
    alone.server.send({ kind: 'browser_ask', id: 2, seat: LEAD, tool: 'browser_close', args: {} });
    await until(() => alone.server.received.length === 1, 'the refusal');
    expect(alone.server.received[0]).toMatchObject({
      kind: 'browser_answer',
      id: 2,
      parts: [],
      error: 'this client cannot host the browser',
    });
  });

  /**
   * **An escalation is re-declared.** A shell that comes up before its host
   * resolves subscribes without the capability and subscribes again once it
   * can host - and the second declare has to reach the server, or the role
   * never moves.
   */
  it('re-declares when the capability is added', async () => {
    const { server, conn } = await connected();
    conn.subscribe(HOME, { browser: false });
    await until(() => server.received.length === 1, 'the first subscribe');
    expect(server.received[0]).toMatchObject({ kind: 'subscribe', browser: false });

    conn.subscribe(HOME, { browser: true });
    await until(() => server.received.length === 2, 'the escalated declare');
    expect(server.received[1]).toMatchObject({ kind: 'subscribe', browser: true });
  });

  /**
   * **The role frame is the only thing that says "you host".** It flips the
   * read and reaches every listener; `takeBrowserRole` puts the claim on the
   * wire; and a drop clears it, because the role belonged to the connection
   * that went rather than to the page.
   */
  it('keeps the role from the server frame, claims it, and clears it on a drop', async () => {
    const { server, conn } = await connected();
    const heard: boolean[] = [];
    conn.onBrowserRole((now) => heard.push(now));
    expect(conn.browserRole(), 'before any frame, this client does not host').toBe(false);

    server.send({ kind: 'browser_role', hosting: true });
    await until(() => conn.browserRole(), 'the grant to land');
    expect(heard, 'the listener heard the grant').toEqual([true]);

    conn.takeBrowserRole();
    await until(() => server.received.length === 1, 'the claim to arrive');
    expect(server.received[0], 'the claim crosses as its own frame').toEqual({
      kind: 'browser_take_role',
    });

    server.drop();
    await until(() => !conn.browserRole(), 'the drop to clear the role');
    expect(heard, 'and the drop is said out loud').toEqual([true, false]);
  });
});

import { type AddressInfo, type RawData, WebSocketServer } from 'ws';
import { afterEach, describe, expect, it } from 'vitest';

import { connect, type Connection } from './socket';
import { slotOf, type ClientMessage, type ServerMessage, type Subject } from './protocol';
import type { SessionSlot } from './wire/types';

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
  server.on('connection', (socket) => {
    socket.on('message', (data) => {
      received.push(JSON.parse(frame(data)) as ClientMessage);
    });
  });

  return {
    url: `ws://127.0.0.1:${port}/socket`,
    received,
    /** Say something to every client attached. */
    send(message: ServerMessage) {
      for (const socket of sockets) socket.send(JSON.stringify(message));
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

const HOME: Subject = 'home';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

const opened: Connection[] = [];
const servers: Awaited<ReturnType<typeof stubServer>>[] = [];

afterEach(async () => {
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
   * subject and an update is one change to it.
   */
  it('holds a snapshot per subject and appends the updates that follow', async () => {
    const { server, conn } = await connected();

    conn.subscribe(HOME);
    await until(() => server.received.length === 1, 'the subscribe to arrive');
    server.send({ kind: 'snapshot', subject: HOME, data: { projects: [] } });
    // A unit variant crosses as its name alone, not as a `kind` field.
    server.send({ kind: 'update', update: 'catalog_loaded' });

    const store = conn.store(HOME);
    await until(() => store?.snapshot() !== null, 'the snapshot to land');
    expect(store?.snapshot()).toEqual({ projects: [] });
    expect(store?.updates()).toHaveLength(1);
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
    expect(server.received[0]).toEqual({ kind: 'subscribe', what: HOME, answering: true });

    // Off unless said otherwise, so a read-only client cannot hang a turn.
    conn.subscribe('usage');
    await until(() => server.received.length === 2, 'the second subscribe');
    expect(server.received[1]).toEqual({ kind: 'subscribe', what: 'usage', answering: false });
  });

  /** The greeting is what configures the client before it draws anything. */
  it('keeps the settings the greeting carried', async () => {
    const { server, conn } = await connected();

    server.send({
      kind: 'greeting',
      version: 1,
      settings: { mark: 'klin', theme: null, font: null },
    });
    await until(() => conn.settings() !== null, 'the greeting to land');
    expect(conn.settings()).toEqual({ mark: 'klin', theme: null, font: null });
  });

  /**
   * A command is the core's own enum, passed through rather than rebuilt:
   * the variant's name around its own fields, which is what the core's
   * externally-tagged enum decodes and what a `kind` field is not.
   *
   * Which of the two envelope shapes it takes is the client's to decide, not
   * the caller's, because the server refuses both mismatches - and it refuses
   * them before dispatching, so getting this wrong is a command that silently
   * never runs rather than one that reports an error.
   */
  it('sets reply_to from the command, and only for the four that answer', async () => {
    const { server, conn } = await connected();

    const cancel = conn.dispatch({ cancel: { key: LEAD } });
    const spawn = conn.dispatch({ spawn_worker: { project_key: 'proj', label: 'w1' } });
    const despawn = conn.dispatch({
      despawn_worker: { project_key: 'proj', label: 'w1', force: false },
    });
    await until(() => server.received.length === 3, 'all three commands');

    const [sentCancel, sentSpawn, sentDespawn] = commands(server.received);
    expect(sentCancel).toMatchObject({ command: { cancel: { key: LEAD } }, reply_to: null });
    expect(sentSpawn).toMatchObject({ command: { spawn_worker: { project_key: 'proj' } } });
    expect(sentDespawn).toMatchObject({ command: { despawn_worker: { label: 'w1' } } });
    // Only the four answer, so only they hand back something to await.
    expect(cancel).toBeNull();
    expect(typeof sentSpawn?.reply_to).toBe('number');
    // Two asks are two channels: one id would hand a caller someone else's answer.
    expect(sentDespawn?.reply_to).not.toEqual(sentSpawn?.reply_to);

    // Nothing here answers the two, and a connection that closes rejects what
    // it was still holding - settled rather than left to reject unhandled.
    conn.close();
    await Promise.allSettled([spawn, despawn]);
  });

  it('asks for more turns of one conversation', async () => {
    const { server, conn } = await connected();

    conn.more(LEAD, 'cursor-1', 20);
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

    server.send({ kind: 'reply', reply_to: sent.reply_to, body: { refused: 'no such worker' } });

    await expect(answer).resolves.toEqual({ refused: 'no such worker' });
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
    expect(server.received[1]).toEqual({ kind: 'subscribe', what: HOME, answering: false });
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
   */
  it('routes an update to the store for the seat it carries', async () => {
    const { server, conn } = await connected();
    const lead: Subject = { session: LEAD };

    conn.subscribe(lead);
    conn.subscribe(HOME);
    await until(() => server.received.length === 2, 'both subscribes');
    server.send({
      kind: 'update',
      update: { turn_cancelled: { key: LEAD } },
    });

    const store = conn.store(lead);
    await until(() => (store?.updates().length ?? 0) === 1, 'the seat to hear it');
    expect(store?.updates()).toEqual([{ turn_cancelled: { key: LEAD } }]);
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

    server.send({ kind: 'error', what: 'subscribe', why: 'forge holds no session for that seat' });
    await until(() => seen.length === 1, 'the error to arrive');
    expect(seen[0]).toEqual({
      kind: 'error',
      what: 'subscribe',
      why: 'forge holds no session for that seat',
    });
  });
});

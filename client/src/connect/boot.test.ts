import { type AddressInfo, WebSocketServer } from 'ws';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { PROTOCOL_VERSION } from '../protocol';
import { DEFAULT_ADDRESS, connectTo, submitAttempt } from './attempt';
import { boot } from './boot';
import { rememberedAddress, rememberAddress } from './remembered';

/** A forge that greets, which is all a launch needs one to do. */
async function stubServer() {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;
  server.on('connection', (socket) => {
    socket.send(
      JSON.stringify({
        kind: 'greeting',
        version: PROTOCOL_VERSION,
        settings: { mark: null, theme: null, font: null },
      }),
    );
  });
  return {
    address: `127.0.0.1:${port}`,
    async close() {
      for (const client of server.clients) client.terminate();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

const servers: Awaited<ReturnType<typeof stubServer>>[] = [];

/**
 * The webview's store, which node does not carry.
 *
 * A browser always has one and `remembered.ts` reads it off the global, so
 * this is what stands in for it here - the real module under test rather
 * than a copy of it with the store handed in.
 */
function openWebviewStore(): void {
  const held = new Map<string, string>();
  const store: Storage = {
    get length() {
      return held.size;
    },
    clear: () => held.clear(),
    getItem: (key) => held.get(key) ?? null,
    key: (index) => [...held.keys()][index] ?? null,
    removeItem: (key) => {
      held.delete(key);
    },
    setItem: (key, value) => {
      held.set(key, value);
    },
  };
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: store });
}

beforeEach(openWebviewStore);

afterEach(async () => {
  Reflect.deleteProperty(globalThis, 'localStorage');
  for (const server of servers.splice(0)) await server.close();
});

describe('what the app opens on', () => {
  /**
   * The whole point of remembering: the app comes back to the forge it last
   * used rather than to a form. A launch that read the address and then drew
   * the door anyway would look exactly like a launch that never read it.
   */
  it('opens the home on the address it remembers, when that address answers', async () => {
    const server = await stubServer();
    servers.push(server);
    rememberAddress(server.address);

    const launched = await boot({ name: 'home' }, rememberedAddress());

    expect(launched.route, 'a remembered address that answered did not open the home').toEqual({
      name: 'home',
    });
    expect(launched.failure).toBeNull();
    expect(launched.connected?.url).toBe(`ws://${server.address}/socket`);
    launched.connected?.connection.close();
  });

  /** A first launch has nothing to open on, so it asks. */
  it('opens the door when it remembers nothing', async () => {
    const launched = await boot({ name: 'home' }, rememberedAddress());

    expect(launched.route, 'a first launch did not open the door').toEqual({ name: 'connect' });
    expect(launched.address, 'the door did not open on the default address').toBe(DEFAULT_ADDRESS);
    expect(launched.connected).toBeNull();
    expect(launched.failure).toBeNull();
  });

  /**
   * The case that matters most: a remembered address that silently does
   * nothing is worse than no memory at all, so the door comes back carrying
   * both the address it tried and the reason it did not answer.
   */
  it('opens the door carrying the reason when the remembered address does not answer', async () => {
    const silent = new WebSocketServer({ port: 0 });
    await new Promise((resolve) => silent.once('listening', resolve));
    const { port } = silent.address() as AddressInfo;
    const address = `127.0.0.1:${port}`;

    try {
      rememberAddress(address);
      // A socket that opens and then says nothing, so the deadline is what
      // settles it: the failure is the same one a refused port gives, without
      // the test waiting out the real one.
      const launched = await boot({ name: 'home' }, rememberedAddress(), (input) =>
        connectTo(input, 20),
      );

      expect(
        launched.route,
        'an address that answered nothing opened something other than the door',
      ).toEqual({ name: 'connect' });
      expect(launched.address, 'the door did not carry the address it tried').toBe(address);
      expect(launched.connected).toBeNull();
      expect(launched.failure?.kind).toBe('unreachable');
      expect(launched.failure?.why).toContain(address);
    } finally {
      for (const client of silent.clients) client.terminate();
      await new Promise((resolve) => silent.close(resolve));
    }
  });

  /**
   * A deep link is how a seat stays reachable, so its page wants the socket
   * the same way the home does - and the ROUTE is what moves the reader, so a
   * launch may take the connection without taking the page.
   */
  it('opens the socket for a deep link without moving off the route it was addressed at', async () => {
    const server = await stubServer();
    servers.push(server);
    rememberAddress(server.address);
    const slot = { org: 'Busytools', project: 'forge', label: 'lead' };

    const launched = await boot({ name: 'session', slot }, rememberedAddress());

    expect(launched.route, 'a launch moved the app off the route it was addressed at').toBeNull();
    expect(
      launched.connected,
      'a deep link was left without the socket its page reads',
    ).not.toBeNull();
    launched.connected?.connection.close();
  });

  /**
   * What is kept is the last address that WORKED, so connecting somewhere
   * else has to replace it rather than sit beside it.
   */
  it('replaces what it remembers when the address is edited and connects', async () => {
    const first = await stubServer();
    const second = await stubServer();
    servers.push(first, second);

    const was = await submitAttempt(first.address, (input) => connectTo(input, 200));
    expect(
      was.connected,
      'the first address did not connect, so there is nothing to replace',
    ).not.toBeNull();
    expect(rememberedAddress(), 'a connection that took was not remembered').toBe(first.address);
    was.connected?.connection.close();

    const now = await submitAttempt(second.address, (input) => connectTo(input, 200));
    expect(now.connected).not.toBeNull();
    expect(rememberedAddress(), 'editing the address left the old one remembered').toBe(
      second.address,
    );
    now.connected?.connection.close();
  });
});

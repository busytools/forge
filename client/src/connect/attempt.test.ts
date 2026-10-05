import { type AddressInfo, WebSocketServer } from 'ws';
import { afterEach, describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { MIN_PROTOCOL, PROTOCOL_VERSION } from '../protocol';
import { DEFAULT_MARK, DEFAULT_WEB_PORT, MARK_NAMES } from '../wire/types';
import {
  DEFAULT_ADDRESS,
  attempt,
  connectTo,
  displayAddress,
  normalizeAddress,
  submitAttempt,
} from './attempt';

/** A forge that greets, which is all this file needs one to do. */
async function stubServer(version = PROTOCOL_VERSION, release: string | null = null) {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;
  server.on('connection', (socket) => {
    socket.send(
      JSON.stringify({
        kind: 'greeting',
        version,
        ...(release === null ? {} : { forge_version: release, forge_version_short: release }),
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

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
});

describe('the address a person types', () => {
  it('takes a bare host and port, and adds the path the server serves', () => {
    expect(normalizeAddress('127.0.0.1:8790')).toEqual({ url: 'ws://127.0.0.1:8790/socket' });
  });

  it('takes the scheme a browser would give, and the socket one', () => {
    expect(normalizeAddress('http://box:8790')).toEqual({ url: 'ws://box:8790/socket' });
    expect(normalizeAddress('https://box:8790')).toEqual({ url: 'wss://box:8790/socket' });
    expect(normalizeAddress('wss://box:8790/socket')).toEqual({ url: 'wss://box:8790/socket' });
  });

  it('keeps a path already given', () => {
    expect(normalizeAddress('box:8790/other')).toEqual({ url: 'ws://box:8790/other' });
  });

  /**
   * Three different refusals, asserted by the words a reader sees. A shared
   * `toHaveProperty('why')` is satisfied by any message at all, so swapping
   * them would be invisible.
   */
  it('refuses an empty field and an address that is not one', () => {
    expect(normalizeAddress('   ')).toEqual({ why: 'Enter the address forge is serving on.' });
    expect(normalizeAddress('::::')).toEqual({
      why: ':::: is not an address. It wants a host and a port, like 127.0.0.1:8790.',
    });
    expect(normalizeAddress('ftp://box:8790')).toEqual({
      why: 'ftp: is not a socket. Use ws:// or wss://, or just 127.0.0.1:8790.',
    });
  });

  it('reads back the host and port the field would show', () => {
    expect(displayAddress(`ws://127.0.0.1:${DEFAULT_WEB_PORT}/socket`)).toBe(
      `127.0.0.1:${DEFAULT_WEB_PORT}`,
    );
  });

  /**
   * What the app holds is the address the person WROTE, which is a bare host
   * and port far more often than a socket URL - and a named host parses as a
   * scheme with an empty host, so a reader who typed `studio:8790` got an
   * empty address on the band and a banner naming nothing.
   */
  it('reads back an address with no scheme, numbered or named', () => {
    expect(displayAddress(DEFAULT_ADDRESS)).toBe(DEFAULT_ADDRESS);
    expect(displayAddress('studio:8790'), 'a named host read back as nothing').toBe('studio:8790');
  });
});

describe('one attempt', () => {
  /**
   * A real socket rejects, and a rejection the caller cannot classify is a
   * page that never leaves "Connecting". It has to arrive as the same arm a
   * refusal does, so the screen draws a reason and re-enables the button.
   */
  it('answers a rejection as an unreachable connection rather than throwing', async () => {
    const answer = await attempt('box:8790', () => Promise.reject(new Error('timed out')));
    expect(answer).toEqual({
      ok: false,
      kind: 'unreachable',
      why: 'Nothing answered at box:8790: timed out',
    });
  });

  it('answers a bad address as an address, not as an unreachable one', async () => {
    const answer = await attempt('::::');
    expect(answer.ok).toBe(false);
    // `address` is the arm with no `[web] enabled` paragraph: the reader can
    // fix this one themselves, so pointing them at their config would be
    // wrong.
    expect(answer.ok === false && answer.kind).toBe('address');
  });

  it('answers a forge that greets with the socket it answered on', async () => {
    const server = await stubServer();
    servers.push(server);

    const answer = await connectTo(server.address);
    expect(answer).toMatchObject({
      ok: true,
      address: server.address,
      settings: { mark: null, theme: null, font: null },
    });
    // The connection is handed back open, not read here: the pages subscribe
    // through it.
    expect(answer.ok && answer.connection.status()).toBe('open');
  });

  /**
   * The greeting carries the protocol, the server fixes it, and it is the
   * only mismatch detector there is - so a client that read it and drew
   * anyway would draw against a shape it cannot know it understands.
   *
   * Above this client's own version is still a refusal: a shipped client
   * cannot be retro-fitted, so that direction has nothing to do but say so.
   */
  it('refuses a forge speaking a protocol this client does not', async () => {
    const server = await stubServer(PROTOCOL_VERSION + 1);
    servers.push(server);

    const answer = await connectTo(server.address);
    expect(answer.ok).toBe(false);
    expect(answer.ok === false && answer.kind).toBe('version');
    expect(answer.ok === false && answer.why).toContain(`protocol ${PROTOCOL_VERSION + 1}`);
    // The ahead direction is this client's to fix: the command it names
    // updates this half, and `just install` - which rebuilds the server, the
    // half that is NOT stale - is not the one offered.
    expect(answer.ok === false && answer.why).toContain('just client-release');
    expect(answer.ok === false && answer.why).not.toContain('just install');
  });

  /**
   * One step back connects: the client has a proven read for it, and a
   * person whose server is a version behind keeps their connection.
   */
  it('carries a forge one step back, with the skew on the connection', async () => {
    const server = await stubServer(MIN_PROTOCOL);
    servers.push(server);

    const answer = await connectTo(server.address);
    expect(answer.ok).toBe(true);
    expect(answer.ok && answer.connection.status()).toBe('open');
    expect(answer.ok && answer.connection.skew()).toEqual({
      serverProtocol: MIN_PROTOCOL,
      serverVersion: null,
    });
  });

  /**
   * Below the floor the connection is refused, and the refusal names both
   * halves and the command - the numbers alone are not something a person
   * can act on.
   */
  it('refuses a forge below the floor, naming the halves and the command', async () => {
    const server = await stubServer(MIN_PROTOCOL - 1);
    servers.push(server);

    const answer = await connectTo(server.address);
    expect(answer.ok).toBe(false);
    expect(answer.ok === false && answer.kind).toBe('version');
    const why = answer.ok === false ? answer.why : '';
    expect(why).toContain(`protocol ${MIN_PROTOCOL - 1}`);
    expect(why).toContain(`protocol ${PROTOCOL_VERSION}`);
    expect(why, 'the refusal named no way out').toContain('just install');
  });

  /**
   * A server old enough to be below the floor predates the greeting's
   * release fields, so the common refusal has no build to name - but a
   * newer one does, and the refusal names it rather than a bare number.
   */
  it('names the release a refused greeting carried, when one crossed', async () => {
    const server = await stubServer(PROTOCOL_VERSION + 1, '1.0.116+def5678');
    servers.push(server);

    const answer = await connectTo(server.address);
    expect(answer.ok).toBe(false);
    const why = answer.ok === false ? answer.why : '';
    expect(why, 'the refusal named the build it was given').toContain('1.0.116+def5678');
  });

  /** A socket that opens and then says nothing is not one this client can draw. */
  it('gives up on a forge that accepts the socket and never greets', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise((resolve) => server.once('listening', resolve));
    const { port } = server.address() as AddressInfo;
    server.on('connection', () => undefined);

    try {
      const answer = await attempt(`127.0.0.1:${port}`, (input) => connectTo(input, 20));
      expect(answer.ok).toBe(false);
      expect(answer.ok === false && answer.kind).toBe('unreachable');
    } finally {
      await new Promise((resolve) => server.close(resolve));
    }
  });
});

describe('one submit, as the screen sees it', () => {
  /**
   * The button is re-enabled on BOTH paths. A transition that only cleared
   * `busy` on the way to the home is a button stuck reading "Connecting"
   * with no reason and no way out, which is indistinguishable from a
   * connection still running.
   */
  it('clears busy and carries the reason when a connection is refused', async () => {
    const next = await submitAttempt('box:8790', () => Promise.reject(new Error('timed out')));
    expect(next.busy, 'a refused connection left the button disabled').toBe(false);
    expect(next.connected).toBeNull();
    expect(next.failure).toEqual({
      ok: false,
      kind: 'unreachable',
      why: 'Nothing answered at box:8790: timed out',
    });
  });

  it('clears busy on a bad address too', async () => {
    const next = await submitAttempt('::::');
    expect(next.busy).toBe(false);
    expect(next.failure?.kind).toBe('address');
    expect(next.connected).toBeNull();
  });

  it('clears busy and carries where to go when the connection took', async () => {
    const server = await stubServer();
    servers.push(server);

    // The production deadline rather than a shortened one: the property here
    // is the pass path, and a tight budget races the machine under load.
    const next = await submitAttempt(server.address, (input) => connectTo(input));
    expect(next.busy).toBe(false);
    expect(next.failure).toBeNull();
    expect(next.connected).toMatchObject({
      address: server.address,
      settings: { mark: null, theme: null, font: null },
    });
  });

  /**
   * The caller writes `busy` from the answer rather than from a `finally`,
   * which holds only while this cannot reject.
   */
  it('never rejects, however the connection failed', async () => {
    const boom = () => Promise.reject(new Error('boom'));
    // A socket that never greets, rather than one that greets a version this
    // client cannot read: the deadline is what makes the promise settle at
    // all, and the point of the test is that every arm settles.
    const silent = new WebSocketServer({ port: 0 });
    await new Promise((resolve) => silent.once('listening', resolve));
    const { port } = silent.address() as AddressInfo;

    try {
      await expect(submitAttempt('box:8790', boom)).resolves.toHaveProperty('busy', false);
      await expect(submitAttempt('::::', boom)).resolves.toHaveProperty('busy', false);
      await expect(
        submitAttempt(`127.0.0.1:${port}`, (input) => connectTo(input, 20)),
      ).resolves.toHaveProperty('busy', false);
    } finally {
      await new Promise((resolve) => silent.close(resolve));
    }
  });
});

describe('the brand marks', () => {
  it('draws every name forge.toml accepts as its own drawing', () => {
    const builtIn = brandPath(DEFAULT_MARK);
    for (const name of MARK_NAMES.filter((name) => name !== DEFAULT_MARK)) {
      expect(brandPath(name), `${name} draws the built-in mark`).not.toBe(builtIn);
    }
  });

  it('draws the built-in for an unset name', () => {
    expect(brandPath(null)).toBe(brandPath(DEFAULT_MARK));
  });
});

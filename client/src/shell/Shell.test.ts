// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { AddressInfo, WebSocket } from 'ws';

import { DEFAULT_ADDRESS } from '../connect/attempt';
import { rememberAddress } from '../connect/remembered';
import { PROTOCOL_VERSION } from '../protocol';
import { fontStack } from '../theme';
import type { ClientSettings } from '../wire/types';
import Shell from './Shell.svelte';

/**
 * A socket and a server that a test in this realm can use, required through
 * node rather than imported.
 *
 * Test mode resolves bare imports with the browser condition, which hands back
 * `ws`'s browser shim - a `WebSocket` and nothing to connect it to - and
 * jsdom's own is built on undici, which builds its events from this realm's
 * `Event` and then has its own event target reject them.
 */
const { WebSocket: Socket, WebSocketServer } = createRequire(import.meta.url)(
  'ws',
) as typeof import('ws');

/**
 * What the shell drew, as a reader reads it.
 *
 * The shell is the only component whose work happens in a launch rather than
 * in a render, so it is the only one `svelte/server` cannot reach: it has no
 * `onMount`, and the route a launch lands on is decided after the first frame.
 */
const drawn = () => document.body.textContent ?? '';

/** The address the door's field is showing, which is what a reader would edit. */
function fieldAddress(): string {
  const input = document.querySelector('#address');
  return input instanceof HTMLInputElement ? input.value : '';
}

let app: Record<string, unknown> | null = null;

/**
 * Let the launch land.
 *
 * A launch is a promise chain, so it needs a task turn rather than a
 * microtask; `flushSync` is what pushes the effects a mount schedules without
 * waiting for one.
 */
async function settle(): Promise<void> {
  flushSync();
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

/** Build the shell at `path`, with `address` kept from last time or nothing. */
async function openAt(path: string, address: string | null): Promise<void> {
  history.replaceState(null, '', path);
  if (address !== null) rememberAddress(address);
  app = mount(Shell, { target: document.body, props: {} });
  await settle();
}

/**
 * A forge that greets, and that a test can hold back from greeting.
 *
 * A launch and a submit only overlap while the launch's attempt is in flight,
 * so a test of that overlap has to hold the far end open rather than answer.
 * An acceptance that then says nothing is what the handshake deadline is for,
 * and this is that window with its end under the test's control.
 */
async function stubForge(settings: ClientSettings, greeting: 'now' | 'held' = 'now') {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;

  /** The first connection it accepts, which for a held launch is the launch's. */
  const attached = new Promise<WebSocket>((resolve) => {
    server.once('connection', resolve);
  });

  const greet = (socket: WebSocket): void => {
    socket.send(JSON.stringify({ kind: 'greeting', version: PROTOCOL_VERSION, settings }));
  };
  server.on('connection', (socket) => {
    if (greeting === 'now') greet(socket);
  });

  return {
    address: `127.0.0.1:${port}`,
    /** How many clients it still has on the end of it. */
    live: () => server.clients.size,
    attached,
    /** Let the greeting go, which is what lands a held launch. */
    greet() {
      for (const socket of server.clients) greet(socket);
    },
    async close() {
      for (const client of server.clients) client.terminate();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

/** Every forge this test opened, closed with the shell. */
const forges: Awaited<ReturnType<typeof stubForge>>[] = [];

/** Type an address into the door and press Connect, as a person would. */
function submit(address: string): void {
  const input = document.querySelector('#address');
  if (!(input instanceof HTMLInputElement)) throw new Error('the door drew no address field');
  input.value = address;
  input.dispatchEvent(new Event('input', { bubbles: true }));
  const form = input.closest('form');
  if (form === null) throw new Error('the door drew no form around the field');
  form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
}

/**
 * Let a greeting cross a real socket and everything it sets off finish.
 *
 * `settle` is one task turn, which is what a launch with no socket under it
 * needs. A greeting comes back through a socket and is followed by a close
 * that has to cross one too, so this gives the event loop turns to deliver
 * both rather than asserting on a frame still in flight.
 */
async function crossed(): Promise<void> {
  for (let turn = 0; turn < 10; turn += 1) {
    flushSync();
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  flushSync();
}

/** The font stack the page is drawing with, which is what a greeting put there. */
function appliedFont(): string {
  return document.documentElement.style.getPropertyValue('--ui');
}

beforeEach(() => {
  Object.defineProperty(globalThis, 'WebSocket', { configurable: true, value: Socket });
});

afterEach(async () => {
  for (const forge of forges.splice(0)) await forge.close();
  if (app !== null) await unmount(app);
  app = null;
  localStorage.clear();
  history.replaceState(null, '', '/');
});

/**
 * Every launch here is given an address this app cannot use, so the failure
 * lands without a socket: what is under test is where the shell puts the
 * reader and what it draws, not the transport.
 */
describe('the shell at launch', () => {
  /**
   * A first launch has nothing to open on, and the door it lands on carries
   * the address the app would use, so one Enter is enough.
   */
  it('opens the door on the default address when it remembers nothing', async () => {
    await openAt('/', null);

    expect(fieldAddress(), 'the door did not open on the default address').toBe(DEFAULT_ADDRESS);
  });

  /**
   * A session URL is how a seat stays reachable, and a launch that moved the
   * reader off it would be a bookmark that lies. The socket is still opened
   * for it; the page is what must not move.
   */
  it('does not move off a route the app was addressed at', async () => {
    await openAt('/session/Busytools/forge/lead', '::::');

    expect(drawn(), 'a launch replaced the page the app was addressed at').toContain(
      'The session page is next',
    );
  });

  /**
   * The door's own URL is where a launch can finish while the door is already
   * on screen, so the reason has to arrive after the fact. A screen that seeds
   * it once is indistinguishable from one where nothing was tried.
   */
  it('draws the launch reason on a door that was already showing', async () => {
    await openAt('/connect', '::::');

    expect(drawn(), 'the door drew no reason for a launch that had failed').toContain(
      'is not an address',
    );
    expect(fieldAddress(), 'the door did not carry the address it tried').toBe('::::');
  });

  /**
   * A submit is a person acting on what is in front of them; a launch is the
   * app guessing. So a launch that lands after a submit has taken a connection
   * yields, rather than taking the page over: without that it closes the
   * socket the submit just opened, rewrites the address and settings, and
   * draws a forge nobody asked for.
   *
   * The launch here is held in flight by a forge that accepts the socket and
   * then says nothing. `/connect` is where that window is reachable, because
   * the door is drawn while the launch behind it is still running.
   */
  it('yields a launch in flight to a connection a submit took', async () => {
    const guessed = await stubForge({ mark: null, theme: null, font: null }, 'held');
    const asked = await stubForge({ mark: null, theme: null, font: 'system' });
    forges.push(guessed, asked);
    await openAt('/connect', guessed.address);
    await guessed.attached;

    submit(asked.address);
    await crossed();
    expect(asked.live(), 'the submit never took, so there is no socket to guard').toBe(1);

    // The launch lands now, into a shell that already holds the person's.
    guessed.greet();
    await crossed();

    expect(asked.live(), 'the launch closed the socket the person opened').toBe(1);
    expect(guessed.live(), 'a launch that yielded left its own socket open').toBe(0);
    expect(appliedFont(), 'the launch rewrote the settings the submit had taken').toBe(
      fontStack('system')?.ui,
    );
  });
});

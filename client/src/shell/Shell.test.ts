// @vitest-environment jsdom
import { createRequire } from 'node:module';

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { AddressInfo, RawData, WebSocket } from 'ws';

import { DEFAULT_ADDRESS } from '../connect/attempt';
import { rememberAddress } from '../connect/remembered';
import { homeWire } from '../dev/fixture.data';
import { MIN_PROTOCOL, PROTOCOL_VERSION } from '../protocol';
import { fontStack } from '../theme';
import { DEFAULT_SETTINGS, type ClientSettings } from '../wire/types';
import type { SessionSlot } from '../wire/types';
import Shell from './Shell.svelte';

/**
 * A socket and a server that a test in this realm can use, required through
 * node rather than imported.
 *
 * Test mode resolves bare imports with the browser condition, and `ws`'s
 * browser shim is one function that throws, so destructuring it gives
 * `undefined` rather than a server. jsdom's own WebSocket is no use either:
 * it is built on undici, which constructs its events from this realm's
 * `Event` and then has its own event target reject them.
 *
 * `socket.test.ts` imports `ws` bare, and can only because that file runs in
 * node. Do not unify the two.
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

/** The text of a socket frame, which every frame these tests send is. */
function frameText(raw: RawData): string {
  if (typeof raw === 'string') return raw;
  if (Array.isArray(raw)) return Buffer.concat(raw).toString('utf8');
  if (Buffer.isBuffer(raw)) return raw.toString('utf8');
  return Buffer.from(raw).toString('utf8');
}

/**
 * A forge that greets, and that a test can hold back from greeting.
 *
 * A launch and a submit only overlap while the launch's attempt is in flight,
 * so a test of that overlap has to hold the far end open rather than answer.
 * An acceptance that then says nothing is what the handshake deadline is for,
 * and this is that window with its end under the test's control.
 */
async function stubForge(
  settings: Partial<ClientSettings>,
  greeting: 'now' | 'held' = 'now',
  version: number = PROTOCOL_VERSION,
) {
  const server = new WebSocketServer({ port: 0 });
  await new Promise((resolve) => server.once('listening', resolve));
  const { port } = server.address() as AddressInfo;

  /** The first connection it accepts, which for a held launch is the launch's. */
  const attached = new Promise<WebSocket>((resolve) => {
    server.once('connection', resolve);
  });

  const greet = (socket: WebSocket): void => {
    socket.send(
      JSON.stringify({
        kind: 'greeting',
        version,
        settings: { ...DEFAULT_SETTINGS, ...settings },
      }),
    );
  };
  server.on('connection', (socket) => {
    // A home read is the fixture, so a page has a roster to reason over; a
    // seat's read is left unanswered, which is the page's own unread state.
    socket.on('message', (raw) => {
      const message: unknown = JSON.parse(frameText(raw));
      const asked = message as { kind?: unknown; what?: unknown };
      if (asked.kind === 'subscribe' && asked.what === 'home') {
        socket.send(JSON.stringify({ kind: 'snapshot', subject: 'home', data: homeWire }));
      }
    });
    if (greeting === 'now') greet(socket);
  });

  return {
    address: `127.0.0.1:${port}`,
    /** How many clients it still has on the end of it. */
    live: () => server.clients.size,
    attached,
    /** Let the greeting go, which is what lands a held launch. */
    greet(again: number = version) {
      for (const socket of server.clients) {
        socket.send(
          JSON.stringify({
            kind: 'greeting',
            version: again,
            settings: { ...DEFAULT_SETTINGS, ...settings },
          }),
        );
      }
    },
    /** Remove a seat, as a lead's cascade or another view's despawn does. */
    remove(seat: SessionSlot, spawnedBy: SessionSlot) {
      for (const socket of server.clients) {
        socket.send(
          JSON.stringify({
            kind: 'update',
            update: {
              worker_status_changed: {
                action: 'removed',
                status: { slot: seat, spawned_by: spawnedBy },
              },
            },
          }),
        );
      }
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
   * for it; the address is what must not move.
   *
   * Nothing answers here, so the door is what the reader meets - which is the
   * same screen the root route falls back to, and why the assertion is the
   * URL rather than the page: a launch from `/` is REWRITTEN to `/connect`,
   * and this one must not be.
   */
  it('does not move off a route the app was addressed at', async () => {
    await openAt('/session/Busytools/forge/lead', '::::');

    expect(location.pathname, 'a launch rewrote the address it was opened at').toBe(
      '/session/Busytools/forge/lead',
    );
    expect(drawn(), 'the door is what a session route draws when nothing answers').toContain(
      'is not an address',
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

describe('a forge that is not the protocol this client speaks', () => {
  /**
   * The floor, seen from the shell: a server one step back is READ rather
   * than refused, and the degradation is never silent - which build is
   * behind, and what to run, are on screen above the page.
   */
  it('runs against a forge one step back and names it', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null }, 'now', MIN_PROTOCOL);
    forges.push(forge);
    await openAt('/', forge.address);
    await crossed();

    expect(location.pathname, 'a forge one step back did not land on the home').toBe('/');
    const said = drawn();
    expect(said, 'the skew named no protocol').toContain(`protocol ${MIN_PROTOCOL}`);
    expect(said, 'the skew named no way out').toContain('just install');
  });

  /**
   * The door draws its own copy - one line, in its own column - and the
   * shell stands down there rather than stacking a second identical strip
   * above it.
   */
  it('draws the skew once on the door', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null }, 'now', MIN_PROTOCOL);
    forges.push(forge);
    await openAt('/connect', forge.address);
    await crossed();

    expect(location.pathname, 'the launch moved the page it was addressed at').toBe('/connect');
    const said = drawn();
    expect(said).toContain('just install');
    expect(said.split('just install').length - 1, 'the skew was drawn twice on one screen').toBe(1);
  });

  /**
   * A connection that STOPS while the door is up is still a refusal, and the
   * door must not turn it into the milder notice: the socket is not coming
   * back, so a reconnect line under it would be a claim nothing is keeping.
   */
  it('keeps a refusal a refusal when the door is the page', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null }, 'now', MIN_PROTOCOL);
    forges.push(forge);
    await openAt('/', forge.address);
    await crossed();
    expect(location.pathname, 'the floor did not land on the home').toBe('/');

    // A later greeting from outside the range stops the connection, as a
    // forge swapped underneath a running client does.
    forge.greet(PROTOCOL_VERSION + 1);
    await crossed();

    history.replaceState(null, '', '/connect');
    dispatchEvent(new PopStateEvent('popstate'));
    await crossed();

    const said = drawn();
    expect(said, 'the refusal was not named on the door').toContain(
      `protocol ${PROTOCOL_VERSION + 1}`,
    );
    expect(said, 'the door claimed a reconnect that nothing is doing').not.toContain(
      'Reconnecting',
    );
    expect(said.split('reinstall').length - 1, 'the refusal was drawn twice').toBe(1);
  });

  /**
   * Below the floor the launch is refused, and the door it lands on carries
   * the refusal: both halves and the command, rather than a number.
   */
  it('stops on a forge below the floor, with the halves and the command shown', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null }, 'now', MIN_PROTOCOL - 1);
    forges.push(forge);
    await openAt('/', forge.address);
    await crossed();

    const said = drawn();
    expect(said).toContain(`protocol ${MIN_PROTOCOL - 1}`);
    expect(said).toContain(`protocol ${PROTOCOL_VERSION}`);
    expect(said, 'the refusal named no way out').toContain('just install');
  });
});

describe('a seat removed under the reader', () => {
  /**
   * The other door onto a landing: the reader closed nothing, but the seat
   * they are on went - a lead's cascade releasing the workers under it, a
   * despawn from another view. The terminal's answer is its
   * `WorkerStatusChanged` handler, and this is the client's: without it the
   * reader sits on a seat with nothing behind it.
   */
  it('moves the reader off a seat the core removes under them', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null });
    forges.push(forge);
    await openAt('/session/TestOrg/proj/w1', forge.address);
    await crossed();
    expect(location.pathname, 'the fixture did not land on the worker').toBe(
      '/session/TestOrg/proj/w1',
    );

    const W1: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'w1' };
    const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };
    forge.remove(W1, LEAD);
    await crossed();

    expect(location.pathname, 'the reader was left on a seat with nothing behind it').toBe(
      '/session/TestOrg/proj/lead',
    );
  });

  /**
   * The other half: a removal somewhere else is not the reader's business,
   * and a page that moved on every update would be unusable.
   *
   * The reader is on the HOME, and the removed seat's own landing is a seat
   * elsewhere - so a watcher that moved on any removal would teleport them
   * into a session, which is the regression this pin has to kill.
   */
  it('leaves the reader where they are when another seat is removed', async () => {
    const forge = await stubForge({ mark: null, theme: null, font: null });
    forges.push(forge);
    await openAt('/', forge.address);
    await crossed();
    expect(location.pathname, 'the fixture did not land on the home').toBe('/');

    forge.remove(
      { org: 'TestOrg', project: 'proj', label: 'w1' },
      { org: 'TestOrg', project: 'proj', label: 'lead' },
    );
    await crossed();

    expect(location.pathname, 'a removal elsewhere moved the reader').toBe('/');
  });
});

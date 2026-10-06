/**
 * Connecting: an address in, and either an open socket or the reason there
 * is not one.
 *
 * The two failures a person can act on are told apart here, because the
 * screen draws them differently: an address this app cannot use, which the
 * reader can fix, and a well-formed address nothing answered on, where the
 * cause is on the far side.
 */

import { hostTheBrowser } from '../browser/host';
import { readableProtocol, skewMessage, skewOf, type Skew } from '../protocol';
import { connect, type Connection } from '../socket';
import { DEFAULT_WEB_PORT, settingsFrom, type ClientSettings } from '../wire/types';
import { rememberAddress } from './remembered';

/**
 * How long a socket has to greet before the address is called unreachable.
 *
 * A server that accepts the connection and then says nothing is not one this
 * client can draw, and a form that waits forever for it is a form that never
 * comes back.
 */
const HANDSHAKE_MS = 5000;

/** The address the form opens on: the loopback port forge serves on. */
export const DEFAULT_ADDRESS = `127.0.0.1:${DEFAULT_WEB_PORT}`;

export type Attempt =
  /**
   * `connection` is the live socket, which the pages read from for as long
   * as the app is open. Nothing bundled stands in for it - the app's only
   * input is the server URL. `address` is that same server as the person
   * wrote it, which is what a field shows them rather than the socket URL.
   */
  | { ok: true; address: string; settings: ClientSettings; connection: Connection }
  /**
   * `address` is an address this app cannot use, and the reader can fix it.
   * `unreachable` is a well-formed address nothing answered on, where the
   * cause is on the far side and the reader needs a pointer rather than a
   * spelling correction. `version` is a forge that answered and speaks a
   * protocol this client does not, which nothing but an upgrade fixes.
   */
  | { ok: false; kind: 'address' | 'unreachable' | 'version'; why: string };

/**
 * The socket URL an address names.
 *
 * A person types `host:port` far more often than a scheme, and a browser
 * gives `http://`. Both are taken, along with the `/socket` path the server
 * serves, so the field accepts what a reader would actually write.
 */
export function normalizeAddress(input: string): { url: string } | { why: string } {
  const trimmed = input.trim();
  if (trimmed === '') return { why: 'Enter the address forge is serving on.' };

  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed) ? trimmed : `ws://${trimmed}`;
  let parsed: URL;
  try {
    parsed = new URL(withScheme);
  } catch {
    return {
      why: `${trimmed} is not an address. It wants a host and a port, like ${DEFAULT_ADDRESS}.`,
    };
  }

  if (parsed.protocol === 'http:') parsed.protocol = 'ws:';
  if (parsed.protocol === 'https:') parsed.protocol = 'wss:';
  if (parsed.protocol !== 'ws:' && parsed.protocol !== 'wss:') {
    return {
      why: `${parsed.protocol} is not a socket. Use ws:// or wss://, or just ${DEFAULT_ADDRESS}.`,
    };
  }
  if (parsed.hostname === '') {
    return { why: `${trimmed} names no host. It wants one, like ${DEFAULT_ADDRESS}.` };
  }
  if (parsed.pathname === '/' || parsed.pathname === '') parsed.pathname = '/socket';

  return { url: parsed.toString() };
}

/**
 * The `host:port` a person reads, for an address the app holds.
 *
 * Both halves reach here: the socket URL a connection was made at, and the
 * address exactly as the person wrote it. The normaliser is what decides what
 * an address IS, so this asks it rather than parsing again - a named host like
 * `studio:8790` otherwise parses as a scheme with an empty host, and read back
 * as an empty address rather than as the one that was typed.
 */
export function displayAddress(address: string): string {
  const normalized = normalizeAddress(address);
  return 'why' in normalized ? address : new URL(normalized.url).host;
}

/**
 * One attempt, and always an `Attempt`: a connection that throws is a
 * connection that did not happen, and the two must reach the screen as the
 * same thing.
 *
 * **A real socket rejects, and this is where that lands.** A handshake
 * timeout, a `WebSocket` constructor that throws on an address the normaliser
 * accepted, a settings round trip that fails: none of them is an exception
 * the page can do anything with, and a caller that let one escape would leave
 * its button disabled reading "Connecting" with no reason and no way out but
 * a reload. So the rejection becomes the `unreachable` arm the screen already
 * knows how to draw.
 *
 * `connect` is a parameter so this can be exercised without a socket; it
 * defaults to the real one.
 */
export async function attempt(
  input: string,
  connect: (input: string) => Promise<Attempt> = connectTo,
): Promise<Attempt> {
  try {
    return await connect(input);
  } catch (error) {
    const why = error instanceof Error ? error.message : String(error);
    return {
      ok: false,
      kind: 'unreachable',
      why: `Nothing answered at ${input.trim()}: ${why}`,
    };
  }
}

/** Where the screen is after one submit, and where it goes if it took. */
export interface Submit {
  busy: boolean;
  failure: Extract<Attempt, { ok: false }> | null;
  connected: Extract<Attempt, { ok: true }> | null;
}

/**
 * One submit, as the screen's next state.
 *
 * The symptom this exists for is a state transition - a button stuck
 * reading "Connecting" with the failure never shown - and a transition taken
 * out of the component is one a test can drive. `svelte/server` renders
 * markup and cannot click, and the client carries no DOM renderer, so a
 * click is not assertable and the decision behind it is.
 *
 * **It cannot reject**, because `attempt` cannot: every path out of a
 * connection, thrown or answered, is one of these two shapes. That is what
 * lets the caller write `busy` from the answer rather than from a `finally`.
 *
 * The address that just took is remembered here rather than by the component,
 * for the same reason the transition is: what the app opens on next time is
 * the last submit that worked, and that is a fact a test can check.
 */
export async function submitAttempt(
  address: string,
  connect: (input: string) => Promise<Attempt> = connectTo,
): Promise<Submit> {
  const answer = await attempt(address, connect);
  if (answer.ok) {
    rememberAddress(address.trim());
    return { busy: false, failure: null, connected: answer };
  }
  return { busy: false, failure: answer, connected: null };
}

/**
 * The greeting, which the server sends before a client has asked for
 * anything.
 *
 * Bounded, because a socket that opens and then says nothing is a server
 * this client cannot draw, and the form would otherwise hold a disabled
 * button forever. Expiry rejects, and `attempt` above turns that into the
 * same `unreachable` arm a refused connection gets.
 */
function greeting(
  connection: Connection,
  handshakeMs: number,
): Promise<{ settings: ClientSettings; version: number; skew: Skew | null }> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      stop();
      reject(new Error(`nothing greeted us within ${handshakeMs / 1000}s`));
    }, handshakeMs);
    const stop = connection.onMessage((message) => {
      if (message.kind !== 'greeting') return;
      clearTimeout(timer);
      stop();
      resolve({
        settings: settingsFrom(message.settings),
        version: message.version,
        skew: skewOf(message),
      });
    });
  });
}

/**
 * Connect, and answer with the socket and what the greeting carried.
 *
 * The connection is handed back open rather than read here: the pages
 * subscribe through it, so what this returns is the channel rather than a
 * picture taken down it.
 */
export async function connectTo(
  input: string,
  handshakeMs: number = HANDSHAKE_MS,
): Promise<Attempt> {
  const normalized = normalizeAddress(input);
  if ('why' in normalized) return { ok: false, kind: 'address', why: normalized.why };

  const connection = connect(normalized.url);
  // The shell is the browser host: a page's subscription declares the
  // capability (see the subscribes), and the asks that follow land here, on
  // their way to the Rust side.
  hostTheBrowser(connection);
  try {
    const { settings, version, skew } = await greeting(connection, handshakeMs);
    // The protocol's only mismatch detector, and the range is the client's
    // own: one step back is read - the connection already carries the skew
    // for whatever draws it - and anything outside the range is refused with
    // the way out named. A `why` of two protocol numbers alone names no
    // build and no way out.
    if (skew !== null && !readableProtocol(version)) {
      connection.close();
      return { ok: false, kind: 'version', why: skewMessage(skew) };
    }
    return { ok: true, address: input.trim(), settings, connection };
  } catch (error) {
    // Nothing is going to draw through this one, and leaving it open would
    // have it reconnect behind a screen that already gave up on it.
    connection.close();
    throw error;
  }
}

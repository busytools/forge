/**
 * Connecting, which is the one thing this file does not do.
 *
 * `connectTo` is the seam the socket lands in, and its signature is the part
 * that stays - a caller hands it an address and reads back settings or a
 * reason, whatever is underneath.
 */

import { loadFixtureHome } from '../dev/fixture';
import type { HomeWire } from '../wire/home';
import { DEFAULT_SETTINGS, DEFAULT_WEB_PORT, type ClientSettings } from '../wire/types';

/** The address the form opens on: the loopback port forge serves on. */
export const DEFAULT_ADDRESS = `127.0.0.1:${DEFAULT_WEB_PORT}`;

export type Attempt =
  /**
   * `wire` is the home snapshot the connection produced, and `null` in a
   * production build: a real one arrives on the socket, which is the other
   * half of this base. Nothing bundled stands in for it - the app's only
   * input is the server URL.
   */
  | { ok: true; url: string; settings: ClientSettings; wire: HomeWire | null }
  /**
   * `address` is an address this app cannot use, and the reader can fix it.
   * `unreachable` is a well-formed address nothing answered on, where the
   * cause is on the far side and the reader needs a pointer rather than a
   * spelling correction.
   */
  | { ok: false; kind: 'address' | 'unreachable'; why: string };

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

/** The `host:port` a person reads, for a socket URL the app holds. */
export function displayAddress(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
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
  connected: { url: string; settings: ClientSettings; wire: HomeWire | null } | null;
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
 */
export async function submitAttempt(
  address: string,
  connect: (input: string) => Promise<Attempt> = connectTo,
): Promise<Submit> {
  const answer = await attempt(address, connect);
  return answer.ok
    ? {
        busy: false,
        failure: null,
        connected: { url: answer.url, settings: answer.settings, wire: answer.wire },
      }
    : { busy: false, failure: answer, connected: null };
}

/**
 * Connect, and answer with what the greeting carried.
 *
 * **The body is the fixture and the signature is not.** Nothing here opens a
 * socket, so this answers as a forge whose `[web]` block names nothing -
 * every mark, palette and typeface at its built-in - and the home behind this
 * screen draws the fixture the server committed. The body becomes
 * `connect(url)` once the pages are wired to it; `attempt` above is what
 * turns its rejections into the screen's own vocabulary, and no caller
 * changes.
 *
 * It resolves rather than being `async`, because this body has nothing to
 * await and `require-await` is right to say so. The signature is the seam
 * all the same: the real body awaits a socket here and changes no caller.
 *
 * The snapshot it carries is the fixture in a DEVELOPMENT build and `null`
 * otherwise, which is what lets the home be looked at without a running
 * forge while shipping nothing. `loadFixtureHome` is the DEV-guarded dynamic
 * import, so a production build has no fixture to reach.
 */
export async function connectTo(input: string): Promise<Attempt> {
  const normalized = normalizeAddress(input);
  if ('why' in normalized) return { ok: false, kind: 'address', why: normalized.why };
  const wire = await loadFixtureHome();
  return { ok: true, url: normalized.url, settings: DEFAULT_SETTINGS, wire };
}

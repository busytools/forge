/**
 * Connecting, which is the one thing this slice does not do.
 *
 * The socket is the other half of the base and it is not here: `connect(url)`
 * belongs with one store per subscription, and the server's protocol is still
 * moving. `connectTo` is the seam it lands in, and its signature is the part
 * that stays - a caller hands it an address and reads back settings or a
 * reason, whatever is underneath.
 */

import { DEFAULT_SETTINGS, DEFAULT_WEB_PORT, type ClientSettings } from '../wire/types';

/** The address the form opens on: the loopback port forge serves on. */
export const DEFAULT_ADDRESS = `127.0.0.1:${DEFAULT_WEB_PORT}`;

export type Attempt =
  | { ok: true; url: string; settings: ClientSettings }
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
    return { why: `${trimmed} is not an address. It wants a host and a port, like ${DEFAULT_ADDRESS}.` };
  }

  if (parsed.protocol === 'http:') parsed.protocol = 'ws:';
  if (parsed.protocol === 'https:') parsed.protocol = 'wss:';
  if (parsed.protocol !== 'ws:' && parsed.protocol !== 'wss:') {
    return { why: `${parsed.protocol} is not a socket. Use ws:// or wss://, or just ${DEFAULT_ADDRESS}.` };
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

/**
 * Connect, and answer with what the greeting carried.
 *
 * **The body is the fixture and the signature is not.** Nothing here opens a
 * socket, so this answers as a forge whose `[web]` block names nothing -
 * every mark, palette and typeface at its built-in - and the home behind this
 * screen draws the fixture the server committed. Task 2 replaces the body
 * with the real `connect(url)`; `attempt` above is what turns its rejections
 * into the screen's own vocabulary, and no caller changes.
 */
export async function connectTo(input: string): Promise<Attempt> {
  const normalized = normalizeAddress(input);
  if ('why' in normalized) return { ok: false, kind: 'address', why: normalized.why };
  return { ok: true, url: normalized.url, settings: DEFAULT_SETTINGS };
}

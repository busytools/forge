import { describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { DEFAULT_MARK, DEFAULT_WEB_PORT, MARK_NAMES } from '../wire/types';
import { homeWire } from '../dev/fixture.data';
import { attempt, displayAddress, normalizeAddress, submitAttempt } from './attempt';

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

  it('passes a connection through untouched', async () => {
    const answer = await attempt('box:8790');
    expect(answer).toEqual({
      ok: true,
      url: 'ws://box:8790/socket',
      settings: { mark: null, theme: null, font: null },
      // The fixture answers in a test environment, which is a DEVELOPMENT
      // build; a production one carries no snapshot until the socket lands.
      wire: homeWire,
    });
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
    const next = await submitAttempt('box:8790');
    expect(next.busy).toBe(false);
    expect(next.failure).toBeNull();
    expect(next.connected).toEqual({
      url: 'ws://box:8790/socket',
      settings: { mark: null, theme: null, font: null },
      wire: homeWire,
    });
  });

  /**
   * The caller writes `busy` from the answer rather than from a `finally`,
   * which holds only while this cannot reject.
   */
  it('never rejects, however the connection failed', async () => {
    const boom = () => Promise.reject(new Error('boom'));
    await expect(submitAttempt('box:8790', boom)).resolves.toHaveProperty('busy', false);
    await expect(submitAttempt('::::', boom)).resolves.toHaveProperty('busy', false);
    await expect(submitAttempt('box:8790')).resolves.toHaveProperty('busy', false);
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

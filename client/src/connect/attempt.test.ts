import { describe, expect, it } from 'vitest';

import { brandPath } from '../brand';
import { DEFAULT_MARK, DEFAULT_WEB_PORT, MARK_NAMES } from '../wire/types';
import { displayAddress, normalizeAddress } from './attempt';

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

  it('refuses an empty field and an address that is not one', () => {
    expect(normalizeAddress('   ')).toHaveProperty('why');
    expect(normalizeAddress('::::')).toHaveProperty('why');
    expect(normalizeAddress('ftp://box:8790')).toHaveProperty('why');
  });

  it('reads back the host and port the field would show', () => {
    expect(displayAddress(`ws://127.0.0.1:${DEFAULT_WEB_PORT}/socket`)).toBe(
      `127.0.0.1:${DEFAULT_WEB_PORT}`,
    );
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

import { describe, expect, it } from 'vitest';

import { hrefFor, hrefForSlot, parseRoute } from './routes';

describe('the URLs the server serves', () => {
  it('resolves the home at the root', () => {
    expect(parseRoute('/')).toEqual({ name: 'home' });
  });

  it('resolves a session to the slot its three segments name', () => {
    expect(parseRoute('/session/Busytools/forge/lead')).toEqual({
      name: 'session',
      slot: { org: 'Busytools', project: 'forge', label: 'lead' },
    });
  });

  it('refuses a session with a segment missing', () => {
    expect(parseRoute('/session/Busytools/forge')).toEqual({ name: 'notFound' });
  });

  it('resolves the connect screen', () => {
    expect(parseRoute('/connect')).toEqual({ name: 'connect' });
  });

  it('refuses a path the server does not serve', () => {
    expect(parseRoute('/sessions')).toEqual({ name: 'notFound' });
    expect(parseRoute('/session/Busytools/forge/lead/extra')).toEqual({ name: 'notFound' });
  });

  it('percent-decodes the segments', () => {
    expect(parseRoute('/session/Busytools/forge/two%20words')).toEqual({
      name: 'session',
      slot: { org: 'Busytools', project: 'forge', label: 'two words' },
    });
  });

  it('round-trips a slot through its URL', () => {
    const route = parseRoute('/session/Busytools/forge/lead');
    expect(route.name).toBe('session');
    if (route.name !== 'session') return;
    expect(parseRoute(hrefFor(route))).toEqual(route);
  });

  /**
   * A row's link is built by `hrefForSlot`, and one encoder is the point:
   * a label with a space or a slash in it has to arrive back as the same
   * label, or the two halves of this module disagree about one address.
   */
  it('escapes a segment that would otherwise change the path', () => {
    const slot = { org: 'Busytools', project: 'forge', label: 'two words/and-ch#1' };
    expect(hrefForSlot(slot)).toBe('/session/Busytools/forge/two%20words%2Fand-ch%231');
    expect(parseRoute(hrefForSlot(slot))).toEqual({ name: 'session', slot });
  });

  /**
   * A bare `%` is not valid percent-encoding and `decodeURIComponent`
   * throws on it. Every popstate parses the URL, so an unguarded decode
   * blanks the whole page rather than one segment.
   */
  it('leaves a segment it cannot percent-decode as it arrived', () => {
    expect(() => parseRoute('/session/O/P/100%')).not.toThrow();
    expect(parseRoute('/session/O/P/100%')).toEqual({
      name: 'session',
      slot: { org: 'O', project: 'P', label: '100%' },
    });
  });
});

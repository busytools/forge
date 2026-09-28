import { describe, expect, it } from 'vitest';

import { hrefFor, parseRoute } from './routes';

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
});

// @vitest-environment jsdom
import { get } from 'svelte/store';
import { describe, expect, it } from 'vitest';

import {
  currentRoute,
  goTo,
  hrefFor,
  hrefForSlot,
  parseRoute,
  titleFor,
  type Route,
} from './routes';

describe('the URLs the server serves', () => {
  it('resolves the home at the root', () => {
    expect(parseRoute('/')).toEqual({ name: 'home' });
  });

  /**
   * A pathname that names the document is the root it was served at.
   *
   * A shell's window url, a hand-typed address or a link can spell
   * `index.html`; parsing that segment as a route lands the launch on the
   * not-a-page screen, which no reader can act on and which #2159's look hit.
   */
  it('reads a pathname carrying the document as the root it was served at', () => {
    expect(parseRoute('/index.html')).toEqual({ name: 'home' });
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

  it('resolves the models page and round-trips it', () => {
    expect(parseRoute('/models')).toEqual({ name: 'models' });
    expect(parseRoute(hrefFor({ name: 'models' }))).toEqual({ name: 'models' });
  });

  it('resolves the extensions page and round-trips it', () => {
    expect(parseRoute('/extensions')).toEqual({ name: 'extensions' });
    expect(parseRoute(hrefFor({ name: 'extensions' }))).toEqual({ name: 'extensions' });
  });

  it('resolves a seat review as a sub-path of the seat and round-trips it', () => {
    // The plausible regression is the bare four-segment path swallowing the
    // fifth: `/session/o/p/l/review` parsing as a label called `l` with a
    // stray segment, which the not-found copy would then have to explain.
    const review = {
      name: 'review',
      slot: { org: 'Busytools', project: 'forge', label: 'lead' },
    } as const;
    expect(parseRoute('/session/Busytools/forge/lead/review')).toEqual(review);
    expect(parseRoute(hrefFor(review))).toEqual(review);
  });

  it('refuses a review path whose last segment is not `review`', () => {
    expect(parseRoute('/session/Busytools/forge/lead/reviewe')).toEqual({ name: 'notFound' });
    expect(parseRoute('/session/Busytools/forge/lead/review/extra')).toEqual({
      name: 'notFound',
    });
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

describe("the tab's title", () => {
  it('names the seat being shown, the project first and a worker by its label', () => {
    expect(titleFor({ name: 'home' }), 'the home is the forge itself').toBe('forge');
    expect(titleFor({ name: 'connect' })).toBe('forge');
    expect(titleFor({ name: 'models' }), 'the models page names what it is').toBe(
      'forge \u{b7} models',
    );
    expect(titleFor({ name: 'extensions' }), 'and the extensions page names itself').toBe(
      'forge \u{b7} extensions',
    );
    expect(
      titleFor({ name: 'session', slot: { org: 'Busytools', project: 'core-v1', label: 'lead' } }),
      "a lead's seat is its project",
    ).toBe('core-v1');
    expect(
      titleFor({ name: 'session', slot: { org: 'Busytools', project: 'core-v1', label: 'w1' } }),
      'a worker adds its own label',
    ).toBe('core-v1 \u{b7} w1');
    expect(
      titleFor({ name: 'review', slot: { org: 'Busytools', project: 'core-v1', label: 'lead' } }),
      'a review says whose it is and what it is',
    ).toBe('core-v1 \u{b7} review');
    expect(
      titleFor({ name: 'review', slot: { org: 'Busytools', project: 'core-v1', label: 'w1' } }),
      'and a worker keeps its label ahead of it',
    ).toBe('core-v1 \u{b7} w1 \u{b7} review');
  });
});

describe('moving the app in place', () => {
  /**
   * **The store is what moves the shell, and the push writes the address.**
   * A `goTo` that only pushed would leave the page on the seat it was
   * showing; one that fired a popstate instead would have the shell
   * re-parse the URL, which drops the fields an address cannot carry.
   */
  it('sets the route store and writes the same address', () => {
    goTo({ name: 'session', slot: { org: 'TestOrg', project: 'proj', label: 'w1' } });

    expect(get(currentRoute)).toEqual({
      name: 'session',
      slot: { org: 'TestOrg', project: 'proj', label: 'w1' },
    });
    expect(location.pathname).toBe('/session/TestOrg/proj/w1');
  });

  /** **A hand-off's own fields cross whole.** `reviewable` and the parked
   *  call's id ride the ask's Open into the route, and the address carries
   *  neither - the store is what keeps them. */
  it('carries the fields the address cannot', () => {
    goTo({
      name: 'doc',
      slot: { org: 'TestOrg', project: 'proj', label: 'lead' },
      path: '/proj/docs/plan.md',
      reviewable: true,
      handoff: 'h-1',
    });

    expect(get(currentRoute), 'the parked id survives the move').toMatchObject({
      reviewable: true,
      handoff: 'h-1',
    });
    expect(location.pathname, 'and the address is the document, without them').toBe(
      '/doc/TestOrg/proj/lead/proj/docs/plan.md',
    );
  });
});

describe('the document route', () => {
  /**
   * The viewer's address IS the document: the seat whose tree the path
   * resolves against, then the resolved absolute path. A hand-off raises
   * this route, and a thread's deep link reopens it, so the two must spell
   * one address.
   */
  it('reads a doc address as its seat and the absolute document path', () => {
    expect(parseRoute('/doc/Busytools/forge/lead/Users/me/Projects/forge/docs/plan.md')).toEqual({
      name: 'doc',
      slot: { org: 'Busytools', project: 'forge', label: 'lead' },
      path: '/Users/me/Projects/forge/docs/plan.md',
      reviewable: false,
    });
  });

  it('addresses a document back to the same path', () => {
    const route: Route = {
      name: 'doc',
      slot: { org: 'Busytools', project: 'forge', label: 'lead' },
      path: '/Users/me/Projects/forge/docs/plan.md',
      reviewable: true,
    };

    const href = hrefFor(route);

    expect(href).toBe('/doc/Busytools/forge/lead/Users/me/Projects/forge/docs/plan.md');
    // The flag is the hand-off's, not the address's: a reopened link is the
    // document, and whether it is reviewable is re-decided where it opens.
    expect(parseRoute(href)).toEqual({ ...route, reviewable: false });
  });

  it('keeps a segment that carries a space or a slash', () => {
    const route: Route = {
      name: 'doc',
      slot: { org: 'My Org', project: 'proj', label: 'lead' },
      path: '/Users/me/My Docs/a plan.md',
      reviewable: false,
    };

    expect(parseRoute(hrefFor(route))).toEqual(route);
  });

  it('names the document in the tab', () => {
    expect(
      titleFor({
        name: 'doc',
        slot: { org: 'Busytools', project: 'forge', label: 'lead' },
        path: '/Users/me/Projects/forge/docs/plan.md',
        reviewable: false,
      }),
    ).toBe('forge \u{b7} plan.md');
  });

  it('is not a document without a path', () => {
    expect(parseRoute('/doc/Busytools/forge/lead')).toEqual({ name: 'notFound' });
  });
});

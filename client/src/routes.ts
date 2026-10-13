/**
 * The app's URLs, which are the ones the server served: the home at the
 * root and a session at `/session/{org}/{project}/{label}`, so a session
 * stays addressable and a deep link works in a browser and a shell alike.
 *
 * The connect screen at `/connect` is the one addition, because the app can
 * point at any forge.
 */

import { writable } from 'svelte/store';

import type { SessionSlot } from './wire/types';

export type Route =
  | { name: 'home' }
  | { name: 'session'; slot: SessionSlot }
  | { name: 'connect' }
  /**
   * One document in the viewer: the seat whose tree its path resolves
   * against, and the resolved absolute path - the document IS the address,
   * so a hand-off and a thread's deep link spell one.
   *
   * `reviewable` and `handoff` are the hand-off's own facts rather than the
   * address's: they ride the ask's Open into this route, and an address read
   * back from the URL carries neither (the read decides `reviewable` again
   * where it opens, and a deep-linked viewer has no parked call to release).
   */
  | {
      name: 'doc';
      slot: SessionSlot;
      path: string;
      reviewable: boolean;
      handoff?: string;
    }
  /** The dictation models: the pins, the catalogue and the updates it proposes. */
  | { name: 'models' }
  /** The extensions: the plugin inventory, its components and the marketplaces. */
  | { name: 'extensions' }
  /**
   * One project's board: its rows, the waiting-on-you pair, and the
   * takeover the tasks row above the composer opens. One project only -
   * nothing of any other project appears on it.
   */
  | { name: 'board'; org: string; project: string }
  /** The seat's review: its rounds, their threads and the loop's state. A
   *  sub-path of the seat, because the seat is what the review is keyed to. */
  | { name: 'review'; slot: SessionSlot }
  /** A development route: the home drawn from the fixture, with no server. */
  | { name: 'fixture' }
  | { name: 'notFound' };

/**
 * A pathname without the document it may name.
 *
 * The routes are the paths the server serves, and the document serving them
 * is `index.html` at the root - which a shell's window url, a hand-typed
 * address or a link may spell out. Dropping that one segment leaves the root
 * it was served at, so such a launch lands on the app rather than on the
 * not-a-page screen, where no reader can act on it.
 */
function withoutDocument(path: string): string {
  const document = '/index.html';
  return path.endsWith(document) ? path.slice(0, -document.length) || '/' : path;
}

/** The route a path names. Anything the server does not serve is `notFound`. */
export function parseRoute(path: string): Route {
  const segments = withoutDocument(path)
    .split('/')
    .filter((segment) => segment !== '');
  if (segments.length === 0) return { name: 'home' };
  if (segments.length === 1 && segments[0] === 'connect') return { name: 'connect' };
  if (segments.length === 1 && segments[0] === 'models') return { name: 'models' };
  if (segments.length === 1 && segments[0] === 'extensions') return { name: 'extensions' };
  if (segments.length === 1 && segments[0] === 'fixture') return { name: 'fixture' };
  if (segments.length === 3 && segments[0] === 'board') {
    const [, org, project] = segments as [string, string, string];
    return { name: 'board', org: decode(org), project: decode(project) };
  }
  if (segments.length === 4 && segments[0] === 'session') {
    const [, org, project, label] = segments as [string, string, string, string];
    return {
      name: 'session',
      slot: { org: decode(org), project: decode(project), label: decode(label) },
    };
  }
  if (segments.length === 5 && segments[0] === 'session' && segments[4] === 'review') {
    const [, org, project, label] = segments as [string, string, string, string, string];
    return {
      name: 'review',
      slot: { org: decode(org), project: decode(project), label: decode(label) },
    };
  }
  if (segments.length >= 5 && segments[0] === 'doc') {
    const [, org, project, label] = segments as [string, string, string, string, ...string[]];
    return {
      name: 'doc',
      slot: { org: decode(org), project: decode(project), label: decode(label) },
      // The path is absolute on the wire (the read answers the resolved
      // one), so its leading slash comes back here rather than from a
      // segment the filter dropped.
      path: `/${segments.slice(4).map(decode).join('/')}`,
      reviewable: false,
    };
  }
  return { name: 'notFound' };
}

/** The path a route is addressed by. */
export function hrefFor(route: Route): string {
  switch (route.name) {
    case 'home':
      return '/';
    case 'connect':
      return '/connect';
    case 'models':
      return '/models';
    case 'extensions':
      return '/extensions';
    case 'fixture':
      return '/fixture';
    case 'board':
      return `/board/${encode(route.org)}/${encode(route.project)}`;
    case 'session':
      return `/session/${encode(route.slot.org)}/${encode(route.slot.project)}/${encode(route.slot.label)}`;
    case 'review':
      return `/session/${encode(route.slot.org)}/${encode(route.slot.project)}/${encode(route.slot.label)}/review`;
    case 'doc': {
      const document = route.path
        .split('/')
        .filter((segment) => segment !== '')
        .map(encode)
        .join('/');
      return `/doc/${encode(route.slot.org)}/${encode(route.slot.project)}/${encode(route.slot.label)}/${document}`;
    }
    case 'notFound':
      return '/';
  }
}

/**
 * A slot's own path, for a link built from a value rather than a route.
 *
 * One encoder for both, so a label with a space or a slash in it cannot
 * produce a URL one half of this module reads back differently from the
 * other.
 */
export function hrefForSlot(slot: SessionSlot): string {
  return `/session/${encode(slot.org)}/${encode(slot.project)}/${encode(slot.label)}`;
}

/**
 * The tab's title: the project of the seat being shown, then a worker's own
 * label. A slot's project is already the name `forge.toml` declares, so it
 * crosses as it stands; everywhere else keeps the plain `forge`.
 */
export function titleFor(route: Route): string {
  if (route.name === 'models') return 'forge \u{b7} models';
  if (route.name === 'extensions') return 'forge \u{b7} extensions';
  if (route.name === 'board') return `forge \u{b7} ${route.project}`;
  if (route.name === 'review') {
    const { project, label } = route.slot;
    const seat = label === 'lead' ? project : `${project} \u{b7} ${label}`;
    return `${seat} \u{b7} review`;
  }
  if (route.name === 'doc') {
    const name = route.path
      .split('/')
      .filter((segment) => segment !== '')
      .pop();
    return name === undefined ? 'forge' : `forge \u{b7} ${name}`;
  }
  if (route.name !== 'session') return 'forge';
  const { project, label } = route.slot;
  return label === 'lead' ? project : `${project} \u{b7} ${label}`;
}

/**
 * The route the app is showing, which is what a component that moves the
 * reader (a close, an ask's Open) reaches without owning the shell's state.
 *
 * **A store rather than a URL round-trip**, because the URL is not the
 * whole route: a hand-off's Open carries a reviewable flag and the parked
 * call's id, and re-parsing the address would drop both. A back or forward
 * is the URL's own move, so the shell re-parses it there - and that is
 * exactly the documented difference: an address read back carries neither.
 */
export const currentRoute = writable<Route>({ name: 'home' });

/**
 * Move the app to `route` in place.
 *
 * The store is what moves it; the push is for the address bar and history.
 * No popstate event rides this: a listener re-parses the URL, which would
 * drop fields the URL cannot carry.
 */
export function goTo(route: Route): void {
  currentRoute.set(route);
  history.pushState(null, '', hrefFor(route));
}

function encode(segment: string): string {
  return encodeURIComponent(segment);
}

function decode(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    // A segment that is not valid percent-encoding is left as it arrived,
    // so a label carrying a bare `%` still resolves to a route rather than
    // throwing the app into a blank page.
    return segment;
  }
}

/**
 * The app's URLs, which are the ones the server served: the home at the
 * root and a session at `/session/{org}/{project}/{label}`, so a session
 * stays addressable and a deep link works in a browser and a shell alike.
 *
 * The connect screen at `/connect` is the one addition, because the app can
 * point at any forge.
 */

import type { SessionSlot } from './wire/types';

export type Route =
  | { name: 'home' }
  | { name: 'session'; slot: SessionSlot }
  | { name: 'connect' }
  | { name: 'notFound' };

/** The route a path names. Anything the server does not serve is `notFound`. */
export function parseRoute(path: string): Route {
  const segments = path.split('/').filter((segment) => segment !== '');
  if (segments.length === 0) return { name: 'home' };
  if (segments.length === 1 && segments[0] === 'connect') return { name: 'connect' };
  if (segments.length === 4 && segments[0] === 'session') {
    const [, org, project, label] = segments as [string, string, string, string];
    return { name: 'session', slot: { org: decode(org), project: decode(project), label: decode(label) } };
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
    case 'session':
      return `/session/${encode(route.slot.org)}/${encode(route.slot.project)}/${encode(route.slot.label)}`;
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

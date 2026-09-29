/**
 * What the app opens on.
 *
 * A remembered address is only a claim that something answered there once, so
 * the launch is an attempt rather than a lookup: it lands on the home, or on
 * the door carrying the reason it did not. Skipping the attempt would either
 * open a page nothing is answering for, or throw the memory away on the first
 * launch where the forge happened to be down.
 */

import type { Route } from '../routes';
import { attempt, connectTo, type Attempt } from './attempt';

/** Where the app lands, and what it carries there. */
export interface Launch {
  /**
   * The route the app moves to, or `null` to stay on the one it was addressed
   * at.
   *
   * **Only a launch from the root moves it.** A session URL is how a seat
   * stays reachable and `/fixture` draws without a server, so a launch that
   * replaced either would be a bookmark that lies. The connection is taken
   * either way - the route moves the reader, the socket does not.
   */
  route: Extract<Route, { name: 'home' | 'connect' }> | null;
  /** The open socket, when the address answered. */
  connected: Extract<Attempt, { ok: true }> | null;
  /** Why it did not, in the vocabulary the door already draws a failure in. */
  failure: Extract<Attempt, { ok: false }> | null;
}

/**
 * Open on the address that was remembered, or on the door.
 *
 * `opened` decides one thing: whether this launch may move the app off the
 * route it was addressed at. `connect` is a parameter for the same reason
 * `attempt` has one - a launch can be exercised without a socket, and the
 * deadline a real one is given can be shortened to the one a test is about.
 *
 * The address itself is not returned: the shell holds it, and starts on
 * `remembered ?? DEFAULT_ADDRESS`, which is the one this would hand back.
 */
export async function boot(
  opened: Route,
  remembered: string | null,
  connect: (input: string) => Promise<Attempt> = connectTo,
): Promise<Launch> {
  const lands = opened.name === 'home';

  if (remembered === null) {
    return { route: lands ? { name: 'connect' } : null, connected: null, failure: null };
  }

  const answer = await attempt(remembered, connect);
  return {
    route: lands ? (answer.ok ? { name: 'home' } : { name: 'connect' }) : null,
    connected: answer.ok ? answer : null,
    failure: answer.ok ? null : answer,
  };
}

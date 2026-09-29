/**
 * What the app opens on.
 *
 * A remembered address is only a claim that something answered there once, so
 * the launch is an attempt rather than a lookup: it lands on the home, or on
 * the door carrying the address it tried and the reason it did not. Skipping
 * the attempt would either open a page nothing is answering for, or throw the
 * memory away on the first launch where the forge happened to be down.
 */

import type { Route } from '../routes';
import { DEFAULT_ADDRESS, attempt, connectTo, type Attempt } from './attempt';

/** Where the app lands, and what it carries there. */
export interface Launch {
  /** The home it reached, or the door. */
  route: Extract<Route, { name: 'home' | 'connect' }>;
  /** What the door's field opens on: the address that was tried, or the default. */
  address: string;
  /** The open socket, when the remembered address answered. */
  connected: Extract<Attempt, { ok: true }> | null;
  /**
   * Why the remembered address did not open the home, in the vocabulary the
   * door already draws a failure in.
   */
  failure: Extract<Attempt, { ok: false }> | null;
}

/**
 * Open on the address that was remembered, or on the door.
 *
 * `connect` is a parameter for the same reason `attempt` has one: a launch can
 * be exercised without a socket, and the deadline a real one is given can be
 * shortened to the one the test is about.
 */
export async function boot(
  remembered: string | null,
  connect: (input: string) => Promise<Attempt> = connectTo,
): Promise<Launch> {
  if (remembered === null) {
    return { route: { name: 'connect' }, address: DEFAULT_ADDRESS, connected: null, failure: null };
  }

  const answer = await attempt(remembered, connect);
  return answer.ok
    ? { route: { name: 'home' }, address: remembered, connected: answer, failure: null }
    : { route: { name: 'connect' }, address: remembered, connected: null, failure: answer };
}

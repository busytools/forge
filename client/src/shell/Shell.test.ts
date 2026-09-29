// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import { DEFAULT_ADDRESS } from '../connect/attempt';
import { rememberAddress } from '../connect/remembered';
import Shell from './Shell.svelte';

/**
 * What the shell drew, as a reader reads it.
 *
 * The shell is the only component whose work happens in a launch rather than
 * in a render, so it is the only one `svelte/server` cannot reach: it has no
 * `onMount`, and the route a launch lands on is decided after the first frame.
 */
const drawn = () => document.body.textContent ?? '';

/** The address the door's field is showing, which is what a reader would edit. */
function fieldAddress(): string {
  const input = document.querySelector('#address');
  return input instanceof HTMLInputElement ? input.value : '';
}

let app: Record<string, unknown> | null = null;

/**
 * Let the launch land.
 *
 * A launch is a promise chain, so it needs a task turn rather than a
 * microtask; `flushSync` is what pushes the effects a mount schedules without
 * waiting for one.
 */
async function settle(): Promise<void> {
  flushSync();
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

/** Build the shell at `path`, with `address` kept from last time or nothing. */
async function openAt(path: string, address: string | null): Promise<void> {
  history.replaceState(null, '', path);
  if (address !== null) rememberAddress(address);
  app = mount(Shell, { target: document.body, props: {} });
  await settle();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  localStorage.clear();
  history.replaceState(null, '', '/');
});

/**
 * Every launch here is given an address this app cannot use, so the failure
 * lands without a socket: what is under test is where the shell puts the
 * reader and what it draws, not the transport.
 */
describe('the shell at launch', () => {
  /**
   * A first launch has nothing to open on, and the door it lands on carries
   * the address the app would use, so one Enter is enough.
   */
  it('opens the door on the default address when it remembers nothing', async () => {
    await openAt('/', null);

    expect(fieldAddress(), 'the door did not open on the default address').toBe(DEFAULT_ADDRESS);
  });

  /**
   * A session URL is how a seat stays reachable, and a launch that moved the
   * reader off it would be a bookmark that lies. The socket is still opened
   * for it; the address is what must not move.
   *
   * Nothing answers here, so the door is what the reader meets - which is the
   * same screen the root route falls back to, and why the assertion is the
   * URL rather than the page: a launch from `/` is REWRITTEN to `/connect`,
   * and this one must not be.
   */
  it('does not move off a route the app was addressed at', async () => {
    await openAt('/session/Busytools/forge/lead', '::::');

    expect(location.pathname, 'a launch rewrote the address it was opened at').toBe(
      '/session/Busytools/forge/lead',
    );
    expect(drawn(), 'the door is what a session route draws when nothing answers').toContain(
      'is not an address',
    );
  });

  /**
   * The door's own URL is where a launch can finish while the door is already
   * on screen, so the reason has to arrive after the fact. A screen that seeds
   * it once is indistinguishable from one where nothing was tried.
   */
  it('draws the launch reason on a door that was already showing', async () => {
    await openAt('/connect', '::::');

    expect(drawn(), 'the door drew no reason for a launch that had failed').toContain(
      'is not an address',
    );
    expect(fieldAddress(), 'the door did not carry the address it tried').toBe('::::');
  });
});

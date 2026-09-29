// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

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

/** Build the shell at `path`, with `address` already kept from last time. */
async function openAt(path: string, address: string): Promise<void> {
  history.replaceState(null, '', path);
  rememberAddress(address);
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
   * A session URL is how a seat stays reachable, and a launch that moved the
   * reader off it would be a bookmark that lies. The socket is still opened
   * for it; the page is what must not move.
   */
  it('does not move off a route the app was addressed at', async () => {
    await openAt('/session/Busytools/forge/lead', '::::');

    expect(drawn(), 'a launch replaced the page the app was addressed at').toContain(
      'The session page is next',
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
  });
});

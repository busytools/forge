// @vitest-environment jsdom
/**
 * Which connection the door dials, pinned.
 *
 * **The whole move turns on this one decision**: a shell whose Rust half
 * holds the socket must dial through it, and a page opened outside one must
 * keep the webview's own socket (that page cannot reach `invoke` at all, and
 * its own socket is also what keeps the browser role unclaimed). Both arms
 * are silent when wrong - the page connects either way, and only the parked
 * webview's fate differs - so it gets its own test rather than a comment.
 */

import { afterEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.hoisted(() =>
  vi.fn((command: string, _args?: unknown): Promise<unknown> => {
    if (command === 'client_state') {
      return Promise.resolve({ status: 'connecting', greeting: null, role: false });
    }
    return Promise.resolve(undefined);
  }),
);
const listen = vi.hoisted(() => vi.fn(() => Promise.resolve(() => undefined)));

vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({ listen }));

import { connectTo } from './attempt';

const marker = (): Record<string, unknown> => window as unknown as Record<string, unknown>;

afterEach(() => {
  invoke.mockClear();
  listen.mockClear();
  delete marker()['__TAURI_INTERNALS__'];
});

describe('the connection the door dials', () => {
  it('a shell dials the Rust half; a plain page keeps the webview socket', async () => {
    // A shell: the marker `canHost` reads.
    marker()['__TAURI_INTERNALS__'] = {};
    // No greeting will come from a dead address, so the door gives up on its
    // own deadline - which is the point: what is asserted is WHICH way it
    // dialled, not what answered.
    await connectTo('127.0.0.1:1', 50).catch(() => undefined);
    const dialed = invoke.mock.calls.find(([command]) => command === 'client_connect');
    const url = (dialed?.[1] as { url?: unknown } | undefined)?.url;
    expect(typeof url, 'the shell asked its own half to dial, with an address').toBe('string');
    expect(listen, 'and listens where its frames will land').toHaveBeenCalled();

    invoke.mockClear();
    delete marker()['__TAURI_INTERNALS__'];
    await connectTo('127.0.0.1:1', 50).catch(() => undefined);
    expect(invoke, 'a page outside the shell never reaches it').not.toHaveBeenCalledWith(
      'client_connect',
      expect.anything(),
    );
  });
});

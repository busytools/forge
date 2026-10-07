// @vitest-environment jsdom
/**
 * The page and the shell agree on the command NAMES.
 *
 * **A one-sided rename fails only as an opaque runtime "failed".** There is
 * no compiler across the `invoke` boundary: the page calls `browser_profiles`
 * while the shell registers something else, and every read dies with a
 * sentence that names neither side. This file is the compiler - each call is
 * driven against a mocked shell and the string on the wire is asserted.
 */

import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest';

const invoke = vi.hoisted(() => vi.fn((): Promise<unknown> => Promise.resolve(undefined)));

vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import {
  answerPart,
  browserUsed,
  bytesOf,
  canHost,
  closeProfile,
  hideBrowser,
  hostTheBrowser,
  listProfiles,
  showBrowser,
  type HostReply,
} from './host';
import type { BrowserAsk } from '../protocol';
import type { Connection } from '../socket';

/**
 * The mapping from what the shell's host returns to what the socket sends,
 * on its own so it can be asserted without a Tauri process: the base64 the
 * host carries is what has to become the bytes of a binary frame, and every
 * screenshot rides it.
 */
describe('what the host answered', () => {
  it('turns a text part into a text part and an image into its bytes', () => {
    const reply: HostReply = {
      parts: [
        { type: 'text', text: 'navigated' },
        { type: 'image', mime_type: 'image/png', data_base64: 'AP8Q' },
      ],
    };
    expect(reply.parts.map(answerPart)).toEqual([
      { type: 'text', text: 'navigated' },
      { type: 'image', mime_type: 'image/png', bytes: new Uint8Array([0x00, 0xff, 0x10]) },
    ]);
  });

  /** The bytes of a base64 string, which is what a binary frame carries. */
  it('decodes the base64 the host carries', () => {
    expect(bytesOf('')).toEqual(new Uint8Array([]));
    expect(bytesOf('AP8Q')).toEqual(new Uint8Array([0x00, 0xff, 0x10]));
  });
});

// **The marker the real shell carries.** Without it `canHost()` answers
// false and every call short-circuits before the invoke - which is the
// module's own guard working, not the thing under test.
beforeAll(() => {
  (window as unknown as Record<string, unknown>)['__TAURI_INTERNALS__'] = {};
});

afterEach(() => {
  invoke.mockClear();
});

describe('the shell command names', () => {
  it('are the ones the page invokes, one call each', async () => {
    const core = await import('@tauri-apps/api/core');
    expect(core.invoke, 'the module under test is the mocked shell').toBe(invoke);
    invoke.mockResolvedValueOnce([]);
    await listProfiles();
    expect(invoke, 'the profiles read').toHaveBeenLastCalledWith('browser_profiles');

    await closeProfile('hunt');
    expect(invoke, "the person's close").toHaveBeenLastCalledWith('browser_profile_close', {
      name: 'hunt',
    });

    await showBrowser();
    expect(invoke, 'the raise').toHaveBeenLastCalledWith('browser_show', { profile: null });

    await showBrowser('hunt');
    expect(invoke, 'a named raise names its profile').toHaveBeenLastCalledWith('browser_show', {
      profile: 'hunt',
    });

    await hideBrowser();
    expect(invoke, 'the lower').toHaveBeenLastCalledWith('browser_hide', { profile: null });

    await hideBrowser('hunt');
    expect(invoke, 'a named lower names its profile').toHaveBeenLastCalledWith('browser_hide', {
      profile: 'hunt',
    });

    await browserUsed();
    expect(invoke, 'the activity mark').toHaveBeenLastCalledWith('browser_used');
  });

  it('and the phone does not claim the capability before its phase', () => {
    const agent = Object.getOwnPropertyDescriptor(window.navigator, 'userAgent');
    Object.defineProperty(window.navigator, 'userAgent', {
      value: 'Mozilla/5.0 (Linux; Android 15; Pixel 9)',
      configurable: true,
    });
    expect(canHost(), 'the phone is not a browser client yet').toBe(false);
    if (agent) Object.defineProperty(window.navigator, 'userAgent', agent);
  });

  /** **The failure arm.** The shell rejects a failed call with the driver's
   * own sentence, and the handler must answer that back rather than let it
   * throw into the socket route - a thrown handler is a park, and the model
   * is left waiting on a call that already failed. */
  it('answers a failed call with its reason', async () => {
    let asked: (ask: BrowserAsk) => Promise<unknown> = () => Promise.resolve(undefined);
    const connection = {
      onBrowserAsk: (fn: (ask: BrowserAsk) => Promise<unknown>) => {
        asked = fn;
        return () => undefined;
      },
    } as unknown as Connection;
    invoke.mockRejectedValueOnce('no browser to drive: install Brave or Google Chrome');
    hostTheBrowser(connection);

    const ask: BrowserAsk = {
      seat: { org: 'o', project: 'p', label: 'l' },
      tool: 'browser_navigate',
      args: {},
      id: 9,
    };
    const answer = await asked(ask);

    expect(answer, 'the reason is the answer, not a throw').toEqual({
      error: 'no browser to drive: install Brave or Google Chrome',
    });
  });

  /** Registration is undoable, and unregistering twice is not a mistake. */
  it('unregisters the handler it registered', () => {
    let asked: ((ask: BrowserAsk) => Promise<unknown>) | null = null;
    const connection = {
      onBrowserAsk: (fn: (ask: BrowserAsk) => Promise<unknown>) => {
        asked = fn;
        return () => {
          asked = null;
        };
      },
    } as unknown as Connection;
    const stop = hostTheBrowser(connection);

    stop();
    expect(asked, 'the handler is gone after the unsubscribe').toBeNull();
    stop();
    expect(asked, 'and unregistering twice is not a mistake').toBeNull();
  });

  it('and the ask rides browser_call with its seat, its tool and its args', async () => {
    let asked: (ask: BrowserAsk) => Promise<unknown> = () => Promise.resolve(undefined);
    const connection = {
      onBrowserAsk: (fn: (ask: BrowserAsk) => Promise<unknown>) => {
        asked = fn;
        return () => undefined;
      },
    } as unknown as Connection;
    hostTheBrowser(connection);
    invoke.mockResolvedValueOnce({ parts: [{ type: 'text', text: 'ok' }] });

    const ask = {
      seat: { org: 'o', project: 'p', label: 'l' },
      tool: 'browser_navigate',
      args: { url: 'https://example.com' },
      id: 7,
    } as unknown as BrowserAsk;
    const answer = await asked(ask);

    expect(invoke, 'the ask names the command and carries the call').toHaveBeenLastCalledWith(
      'browser_call',
      {
        seat: { org: 'o', project: 'p', label: 'l' },
        tool: 'browser_navigate',
        args: { url: 'https://example.com' },
      },
    );
    expect(answer, 'and the parts come back mapped').toEqual({
      parts: [{ type: 'text', text: 'ok' }],
    });
  });
});

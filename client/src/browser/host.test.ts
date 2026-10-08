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
  installTakeoverHook,
  listProfiles,
  showBrowser,
  type HostReply,
} from './host';
import { browserInflight, callDown, callUp } from './inflight.svelte';
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
    expect(
      reply.parts.map(answerPart),
      'every part crosses as itself: text as text, an image as the bytes of its base64',
    ).toEqual([
      { type: 'text', text: 'navigated' },
      { type: 'image', mime_type: 'image/png', bytes: new Uint8Array([0x00, 0xff, 0x10]) },
    ]);
  });

  /** The bytes of a base64 string, which is what a binary frame carries. */
  it('decodes the base64 the host carries', () => {
    expect(bytesOf(''), 'an empty answer carries no bytes').toEqual(new Uint8Array([]));
    expect(
      bytesOf('AP8Q'),
      'the base64 the host carries decodes to the exact bytes, not to a re-encoding',
    ).toEqual(new Uint8Array([0x00, 0xff, 0x10]));
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
  browserInflight.calls = 0;
  browserInflight.visible = false;
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

  it('and the phone claims the capability like any other shell', () => {
    // The Android phase landed: the phone's engine is its own system WebView
    // and the driver runs in-app (issue #1839). The page is the same shell
    // either way, so the UA decides nothing but iOS's absence - see below.
    const agent = Object.getOwnPropertyDescriptor(window.navigator, 'userAgent');
    Object.defineProperty(window.navigator, 'userAgent', {
      value: 'Mozilla/5.0 (Linux; Android 15; Pixel 9)',
      configurable: true,
    });
    expect(canHost(), 'the phone hosts the browser now').toBe(true);
    if (agent) Object.defineProperty(window.navigator, 'userAgent', agent);
  });

  it('and a page that is no shell at all never claims it', () => {
    // **The marker is the capability's one gate.** A mutant dropping the
    // `__TAURI_INTERNALS__` check would claim the role in a plain browser
    // and fail every ask routed to it.
    const internals = Object.getOwnPropertyDescriptor(window, '__TAURI_INTERNALS__');
    delete (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    expect(canHost(), 'outside the shell there is no host to declare').toBe(false);
    if (internals) Object.defineProperty(window, '__TAURI_INTERNALS__', internals);
  });

  it('and an iOS shell does not claim what it cannot serve', () => {
    // No iOS build exists; if one lands, it must not hold the exclusive
    // browser role with no host behind it (this is why the capability is
    // declared only where a host is really there).
    const agent = Object.getOwnPropertyDescriptor(window.navigator, 'userAgent');
    Object.defineProperty(window.navigator, 'userAgent', {
      value: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)',
      configurable: true,
    });
    expect(canHost(), 'an iOS shell has no browser host to answer with').toBe(false);
    if (agent) Object.defineProperty(window.navigator, 'userAgent', agent);
  });

  it("and the takeover hook's literals are the ones the phone's bar sends", () => {
    // The bar (Kotlin) and this page (TS) meet on strings no compiler
    // checks: `window.__forgeTakeover('done' | 'lowered' | 'raised')` and
    // the `forge-takeover` CustomEvent. A one-sided rename would make the
    // bar silently dead.
    const events: string[] = [];
    const listen = (event: Event) => events.push((event as CustomEvent<string>).detail);
    window.addEventListener('forge-takeover', listen);
    installTakeoverHook();

    (window as unknown as { __forgeTakeover: (what: string) => void }).__forgeTakeover('done');
    (window as unknown as { __forgeTakeover: (what: string) => void }).__forgeTakeover('lowered');

    window.removeEventListener('forge-takeover', listen);
    expect(events, 'the three literals cross as themselves').toEqual(['done', 'lowered']);
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

  /** **The live mark**: up while the call runs, down the moment it answers -
   * which is what makes the strip's ring mean "working right now" rather
   * than "has worked at some point". */
  it('marks the browser in flight while an ask runs', async () => {
    let asked: (ask: BrowserAsk) => Promise<unknown> = () => Promise.resolve(undefined);
    const connection = {
      onBrowserAsk: (fn: (ask: BrowserAsk) => Promise<unknown>) => {
        asked = fn;
        return () => undefined;
      },
    } as unknown as Connection;
    hostTheBrowser(connection);
    let during = -1;
    invoke.mockImplementationOnce(() => {
      // Read from inside the call: exactly the moment the ring claims.
      during = browserInflight.calls;
      return Promise.resolve({ parts: [] });
    });

    const ask = {
      seat: { org: 'o', project: 'p', label: 'l' },
      tool: 'browser_navigate',
      args: { url: 'https://example.com' },
      id: 8,
    } as unknown as BrowserAsk;
    await asked(ask);

    expect(during, 'the mark was up while the call ran').toBe(1);
    expect(browserInflight.calls, 'and down the moment it answered').toBe(0);
  });

  /** **The tail re-arms from each landing**: a burst stays up as one working
   *  stretch, and only a real pause takes the ring down. */
  it('keeps the working mark up through a burst, measured from each landing', () => {
    vi.useFakeTimers();
    try {
      callUp();
      callDown();
      expect(browserInflight.visible, 'up after the first landing').toBe(true);
      vi.advanceTimersByTime(399);
      callUp();
      callDown();
      vi.advanceTimersByTime(399);
      expect(browserInflight.visible, 'a landing inside the tail re-arms it').toBe(true);
      vi.advanceTimersByTime(2);
      expect(browserInflight.visible, 'and the ring comes down a tail past the last landing').toBe(
        false,
      );
    } finally {
      vi.useRealTimers();
    }
  });
});

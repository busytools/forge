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

const invoke = vi.hoisted(() => vi.fn(async (): Promise<unknown> => undefined));

vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import {
  browserUsed,
  canHost,
  closeProfile,
  hideBrowser,
  hostTheBrowser,
  listProfiles,
  showBrowser,
} from './host';
import type { BrowserAsk } from '../protocol';
import type { Connection } from '../socket';

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
    expect(invoke, 'the raise').toHaveBeenLastCalledWith('browser_show');

    await hideBrowser();
    expect(invoke, 'the lower').toHaveBeenLastCalledWith('browser_hide');

    await browserUsed();
    expect(invoke, 'the activity mark').toHaveBeenLastCalledWith('browser_used');
  });

  it('and the phone does not claim the capability before its phase', async () => {
    const agent = Object.getOwnPropertyDescriptor(window.navigator, 'userAgent');
    Object.defineProperty(window.navigator, 'userAgent', {
      value: 'Mozilla/5.0 (Linux; Android 15; Pixel 9)',
      configurable: true,
    });
    expect(canHost(), 'the phone is not a browser client yet').toBe(false);
    if (agent) Object.defineProperty(window.navigator, 'userAgent', agent);
  });

  it('and the ask rides browser_call with its seat, its tool and its args', async () => {
    let asked: ((ask: BrowserAsk) => Promise<unknown>) | null = null;
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
    const answer = await asked?.(ask);

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

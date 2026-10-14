/**
 * The shell capture's module load on an insecure origin.
 *
 * `crypto.randomUUID` is secure-context-only, and the web half is served
 * over plain http on a LAN. Node and jsdom both supply it, so only a case
 * that removes it can see a module-scope call throw - and the import chain
 * is static from the shell down to this module, so it takes the app's whole
 * draw with it.
 */

import { describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (): Promise<undefined> => Promise.resolve(undefined),
  isTauri: (): boolean => false,
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (): Promise<() => void> => Promise.resolve(() => {}),
}));

describe('the shell capture on a plain-http origin', () => {
  it('loads with crypto.randomUUID absent, as that origin is', async () => {
    const real = globalThis.crypto;
    vi.stubGlobal('crypto', { getRandomValues: real.getRandomValues.bind(real) });
    try {
      // Imported here rather than at the top, so the module is evaluated
      // with the stub in place - which is the whole of what this pins.
      const shell = await import('./mic-shell');
      expect(
        typeof shell.openShellMic,
        'a throw at this module load renders nothing at all: the shell draws off a graph that includes it',
      ).toBe('function');
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

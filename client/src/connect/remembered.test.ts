import { afterEach, describe, expect, it, vi } from 'vitest';

import { rememberedAddress, rememberAddress } from './remembered';

/**
 * A webview whose store refuses every call, which is what one with storage
 * turned off does - it throws on the call rather than answering empty.
 */
function refusingStore(): void {
  const refuse = () => {
    throw new Error('storage is off');
  };
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    value: { getItem: refuse, setItem: refuse },
  });
}

afterEach(() => {
  Reflect.deleteProperty(globalThis, 'localStorage');
  vi.restoreAllMocks();
});

describe('a webview that will not keep the address', () => {
  /**
   * The read runs while the shell is being built, so a store call that throws
   * takes the whole app with it: no page at all, not even the door, which is
   * strictly worse than one that forgot where it had been.
   */
  it('answers nothing rather than throwing when the store refuses to be read', () => {
    refusingStore();

    expect(rememberedAddress(), 'a refused read took the app down with it').toBeNull();
  });

  /**
   * The write happens on a connection that just took, so the reader is on the
   * home and sees nothing. Without a record, the next launch is on the door
   * with the default address and nothing anywhere saying why - this feature's
   * own failure mode with no visibility at all.
   */
  it('says so when the store refuses to keep the address', () => {
    refusingStore();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});

    expect(() => {
      rememberAddress('127.0.0.1:8790');
    }, 'a refused write took the app down with it').not.toThrow();
    expect(warn, 'a refused write was recorded nowhere').toHaveBeenCalled();
  });
});

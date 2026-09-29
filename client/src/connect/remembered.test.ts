import { afterEach, describe, expect, it, vi } from 'vitest';

import { rememberedAddress, rememberAddress } from './remembered';

/**
 * A webview whose store refuses every call, by throwing on it rather than
 * answering empty.
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

/**
 * A webview that will not hand the store over at all.
 *
 * The other shape, and the one browsers actually produce: with site data
 * blocked the GETTER throws, so the store is never reached and both callers
 * fall through their `?.` unless the guard itself says something.
 */
function refusingGetter(): void {
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    get() {
      throw new Error('site data is blocked');
    },
  });
}

/** The console line the guard leaves, if it leaves one. */
function watchTheConsole() {
  return vi.spyOn(console, 'warn').mockImplementation(() => {});
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
    const warn = watchTheConsole();

    expect(rememberedAddress(), 'a refused read took the app down with it').toBeNull();
    expect(warn, 'a refused read was recorded nowhere').toHaveBeenCalled();
  });

  /**
   * The write happens on a connection that just took, so the reader is on the
   * home and sees nothing. Without a record, the next launch is on the door
   * with the default address and nothing anywhere saying why - this feature's
   * own failure mode with no visibility at all.
   */
  it('says so when the store refuses to keep the address', () => {
    refusingStore();
    const warn = watchTheConsole();

    expect(() => {
      rememberAddress('127.0.0.1:8790');
    }, 'a refused write took the app down with it').not.toThrow();
    expect(warn, 'a refused write was recorded nowhere').toHaveBeenCalled();
  });

  /**
   * Reaching the store is its own step, and the one most likely to be
   * refused. A guard that swallows it silently leaves exactly the state this
   * module exists to avoid: an app that forgets and never says why.
   */
  it('says so when the store cannot be reached at all', () => {
    refusingGetter();
    const warn = watchTheConsole();

    expect(rememberedAddress(), 'a refused getter took the app down with it').toBeNull();
    expect(warn, 'a store that could not be reached was recorded nowhere').toHaveBeenCalled();
  });
});

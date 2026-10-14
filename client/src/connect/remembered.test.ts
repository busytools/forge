import { afterEach, describe, expect, it, vi } from 'vitest';

import { movedParameter, rememberedAddress, rememberAddress, serverParameter } from './remembered';

describe('the server a launch names', () => {
  /**
   * **The parameter is how a client is opened already pointed at a forge**:
   * the door pre-fills from it and the launch tries it, so a scratch stack
   * needs no typed address. It outranks the remembered one, because it is
   * this launch's own instruction.
   */
  it('reads the server parameter, and nothing when there is none', () => {
    expect(serverParameter('?server=127.0.0.1:8795'), 'a bare host:port reads').toBe(
      '127.0.0.1:8795',
    );
    expect(serverParameter('?server=ws://127.0.0.1:8795/socket'), 'a full URL reads').toBe(
      'ws://127.0.0.1:8795/socket',
    );
    expect(serverParameter('?server=%20127.0.0.1:8795%20'), 'whitespace is trimmed').toBe(
      '127.0.0.1:8795',
    );
    expect(serverParameter(''), 'nothing named').toBeNull();
    expect(serverParameter('?other=1'), 'another parameter is not this one').toBeNull();
    expect(serverParameter('?server='), 'an empty value names nothing').toBeNull();
    expect(serverParameter('?server=   '), 'blank whitespace names nothing too').toBeNull();
  });

  /**
   * **The parameter follows the connection the tab took.** A submit that
   * corrects a stale parameter has moved the tab, and a URL left naming the
   * address it replaced would outrank the live one at the next launch.
   */
  it('moves the server parameter to an address that took', () => {
    const moved = movedParameter('?server=127.0.0.1:8791', '127.0.0.1:8803');
    expect(moved, 'the parameter did not follow the connection').not.toBeNull();
    expect(moved?.[0], 'the moved search lost its leading ?').toBe('?');
    expect(
      serverParameter(moved ?? ''),
      'reading the moved parameter back did not yield the address that took',
    ).toBe('127.0.0.1:8803');

    const withAnother = movedParameter('?a=1&server=dead:1', 'live:2');
    expect(
      serverParameter(withAnother ?? ''),
      'the moved parameter does not read back as the address that took',
    ).toBe('live:2');
    expect(
      new URLSearchParams(withAnother ?? '').get('a'),
      'another parameter was dropped in the move',
    ).toBe('1');

    expect(
      movedParameter('?server=127.0.0.1:8803', '127.0.0.1:8803'),
      'a URL already naming the connection was rewritten',
    ).toBeNull();
    expect(
      movedParameter('', '127.0.0.1:8803'),
      'a URL naming no server grew a parameter',
    ).toBeNull();
  });
});

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

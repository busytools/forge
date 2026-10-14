/**
 * The address the app comes back to.
 *
 * The last address that ANSWERED is kept in the webview's own store and read
 * on launch, so the app opens on the forge it last used rather than on a form.
 * Nothing else is kept: an address that answered nothing is not one to open on
 * next time, and forgetting it would also throw away the one a person is about
 * to fix and try again.
 *
 * **Every call on the store is caught, not just the read of the global.** A
 * browser with storage turned off throws on the call, and the read happens
 * while the shell is being built - so an unguarded `getItem` is not a missing
 * memory, it is an app with no page at all.
 */

import { report } from '../socket';

/** Where the webview keeps it. */
const KEY = 'forge.address';

/** Where a launch names a server, on the app's own URL. */
const PARAM = 'server';

/**
 * The address a launch names, or `null` when none does.
 *
 * **The door pre-fills from it**, so an app can be opened already pointed at
 * a server - a scratch forge, a second machine - without typing:
 * `?server=127.0.0.1:8795`, or a full ws:// URL. A parameter is an
 * instruction for THIS launch, so it outranks the address the app would
 * otherwise open on. Nothing here validates the value: the door's own submit
 * does, and a bad address belongs in the door's refusal rather than dropped
 * in silence.
 */
export function serverParameter(search: string): string | null {
  const value = new URLSearchParams(search).get(PARAM)?.trim() ?? '';
  return value === '' ? null : value;
}

/**
 * The search that names `address`, when the URL's parameter names another
 * server; `null` when the URL names no server, or already names this one.
 *
 * A connection taken on another address has MOVED the tab a `?server=`
 * launch pointed at, so the parameter follows it - otherwise the address it
 * replaced would outrank the connection at the next launch and dial it
 * again. A URL naming no server is left alone: the address that just took
 * is remembered, and inventing a parameter would write this launch's guess
 * into the address bar.
 */
export function movedParameter(current: string, address: string): string | null {
  const named = serverParameter(current);
  if (named === null || named === address) return null;
  const search = new URLSearchParams(current);
  search.set(PARAM, address);
  return `?${search.toString()}`;
}

/**
 * The webview's own store, or `null` where there is not one.
 *
 * Reaching the store is its own step and the one most likely to be refused:
 * with site data blocked the GETTER throws, so the store is never reached and
 * both callers fall through their `?.`. A guard that swallowed that silently
 * would leave exactly the state this module exists to avoid.
 */
function webview(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch (why) {
    report('could not reach the webview store', why);
    return null;
  }
}

/**
 * The last address that answered, or `null` when none has.
 *
 * A store that refuses to be read is the same as one that never held
 * anything: this app's only input is the server URL, so an app that cannot
 * remember is one that asks.
 */
export function rememberedAddress(): string | null {
  try {
    return webview()?.getItem(KEY) ?? null;
  } catch (why) {
    report('could not read the remembered address', why);
    return null;
  }
}

/**
 * Keep the address that just answered, as the one the app opens on.
 *
 * A store that refuses the write is not something the reader can act on mid
 * session - the connection in front of them is unaffected, and the cost lands
 * on the next launch, which will ask again. That is exactly why it is
 * recorded: without a line somewhere, a refusal here is indistinguishable
 * from a feature that never worked.
 */
export function rememberAddress(address: string): void {
  try {
    webview()?.setItem(KEY, address);
  } catch (why) {
    report('could not remember the address', why);
  }
}

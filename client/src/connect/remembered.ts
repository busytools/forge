/**
 * The address the app comes back to.
 *
 * The last address that ANSWERED is kept in the webview's own store and read
 * on launch, so the app opens on the forge it last used rather than on a form.
 * Nothing else is kept: an address that answered nothing is not one to open on
 * next time, and forgetting it would also throw away the one a person is about
 * to fix and try again.
 */

/** Where the webview keeps it. */
const KEY = 'forge.address';

/**
 * The webview's own store, or `null` where there is not one.
 *
 * Reading the global throws rather than answering `undefined` in a browser
 * with storage turned off, and an app that could not reach its own memory is
 * the same as one that never had any.
 */
function webview(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

/** The last address that answered, or `null` when none has. */
export function rememberedAddress(): string | null {
  return webview()?.getItem(KEY) ?? null;
}

/**
 * Keep the address that just answered, as the one the app opens on.
 *
 * A store that refuses the write is not something the reader can act on: the
 * session in front of them is unaffected, and the cost is that the next launch
 * asks for the address again.
 */
export function rememberAddress(address: string): void {
  try {
    webview()?.setItem(KEY, address);
  } catch {
    // Nothing to do about it, and nothing the reader could do either.
  }
}

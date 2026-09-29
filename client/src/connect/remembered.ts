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

/** Where the webview keeps it. */
const KEY = 'forge.address';

/** The webview's own store, or `null` where there is not one. */
function webview(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
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
    say('could not read the remembered address', why);
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
    say('could not remember the address', why);
  }
}

/**
 * Say so on the console, which is the client's only channel of its own.
 *
 * Deliberately not a logging framework: what it reports is a fact a reader of
 * the console can act on, and the alternative is silence.
 */
function say(what: string, why: unknown): void {
  console.warn(`forge client: ${what}`, why);
}

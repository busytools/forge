/**
 * A fresh prompt id, in whatever shape the platform can actually mint one.
 *
 * `crypto.randomUUID` is secure-context-only: on a plain-http origin - which
 * is how this client is reached over a LAN - it is `undefined`, and calling
 * it takes the whole send down inside the try that exists for a closed
 * socket. `getRandomValues` is available in insecure contexts too, so it is
 * the first fallback; a webview with neither gets a counter, which is all the
 * requirement is - ids have to be UNIQUE, because the CLI reads a repeat as a
 * duplicate and drops that prompt silently.
 */

let counted = 0;

export function mintPromptId(): string {
  const web = globalThis.crypto;
  if (typeof web?.randomUUID === 'function') {
    return web.randomUUID();
  }
  if (typeof web?.getRandomValues === 'function') {
    const bytes = new Uint8Array(16);
    web.getRandomValues(bytes);
    return `p-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')}`;
  }
  counted += 1;
  return `p-${Date.now().toString(16)}-${counted.toString(16)}`;
}

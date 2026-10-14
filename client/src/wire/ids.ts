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

/**
 * A fresh dispatch id: the token one press's answers name back.
 *
 * A random 53-bit integer - the largest a JS number holds exactly, so it
 * crosses as the integer it is - rather than a counter, because the outcome
 * frames reach every client watching the page: a per-page counter would
 * collide across two clients, and one client's answer would then dispose the
 * other's draft. `Math.random` is the fallback, weaker but present wherever
 * `getRandomValues` is not.
 */
export function mintDispatchId(): number {
  const web = globalThis.crypto;
  if (typeof web?.getRandomValues === 'function') {
    const [high, low] = web.getRandomValues(new Uint32Array(2));
    // 21 bits from the low word keeps the sum under 2^53.
    return (high ?? 0) * 2 ** 21 + ((low ?? 0) % 2 ** 21);
  }
  return Math.floor(Math.random() * 2 ** 53);
}

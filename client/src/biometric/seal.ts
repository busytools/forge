/**
 * AES-256-GCM sealing over base64 text, on the platform's own WebCrypto.
 *
 * **Two kinds of caller, one primitive**: a gate that wraps the key it holds
 * under a passkey-derived key, and a consumer that seals its material under
 * that key. Nothing here knows what a purpose is or what the text means.
 *
 * **The buffer is an `ArrayBuffer`, not a `SharedArrayBuffer`.** WebCrypto's
 * `BufferSource` refuses a shared one, and a plain `new Uint8Array` is typed
 * as possibly shared.
 */

const IV_BYTES = 12;

/** The WebCrypto this webview carries, or null when it has none. */
function subtleCrypto(): Crypto | null {
  const held = globalThis.crypto;
  if (held === undefined || held.subtle === undefined) return null;
  return held;
}

/** A random byte string, or null when the webview has no random source. */
export function randomBytes(length: number): Uint8Array<ArrayBuffer> | null {
  const crypto = globalThis.crypto;
  if (crypto === undefined) return null;
  const bytes = fresh(length);
  crypto.getRandomValues(bytes);
  return bytes;
}

/** Seal `text` under `key`: base64 of `iv || ciphertext`. */
export async function sealText(text: string, key: Uint8Array<ArrayBuffer>): Promise<string | null> {
  const crypto = subtleCrypto();
  if (crypto === null) return null;
  const iv = randomBytes(IV_BYTES);
  if (iv === null) return null;
  try {
    const imported = await crypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt']);
    const encoded = new TextEncoder().encode(text);
    const plain = fresh(encoded.length);
    plain.set(encoded);
    const ciphertext = new Uint8Array(
      await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, imported, plain),
    );
    const sealed = fresh(iv.length + ciphertext.length);
    sealed.set(iv, 0);
    sealed.set(ciphertext, iv.length);
    return toBase64(sealed);
  } catch {
    return null;
  }
}

/** The text back, or null when the key is wrong or the bytes are not ours. */
export async function openText(
  sealed: string,
  key: Uint8Array<ArrayBuffer>,
): Promise<string | null> {
  const crypto = subtleCrypto();
  if (crypto === null) return null;
  const bytes = fromBase64(sealed);
  if (bytes === null || bytes.length <= IV_BYTES) return null;
  try {
    const imported = await crypto.subtle.importKey('raw', key, 'AES-GCM', false, ['decrypt']);
    const plain = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: bytes.subarray(0, IV_BYTES) },
      imported,
      bytes.subarray(IV_BYTES),
    );
    return new TextDecoder().decode(plain);
  } catch {
    return null;
  }
}

/**
 * A fresh byte array whose buffer is an `ArrayBuffer`, which is the shape
 * WebCrypto accepts.
 */
export function fresh(length: number): Uint8Array<ArrayBuffer> {
  return new Uint8Array(new ArrayBuffer(length));
}

export function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function fromBase64(value: string): Uint8Array<ArrayBuffer> | null {
  try {
    const binary = atob(value);
    const bytes = fresh(binary.length);
    for (let at = 0; at < binary.length; at += 1) {
      bytes[at] = binary.charCodeAt(at);
    }
    return bytes;
  } catch {
    return null;
  }
}

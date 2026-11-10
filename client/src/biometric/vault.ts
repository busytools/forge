/**
 * The vault's slice of the biometric proxy: the purpose it enrols under, the
 * material a start needs, and the bytes the core stores.
 *
 * **Two items, two owners.** The gate holds a random wrapping key under the
 * platform authenticator; the core holds the material sealed under that key.
 * The split is what satisfies the brief's two requirements at once - the core
 * stores a copy it cannot read, and any client on this device that passes the
 * gesture can open it. The web build keeps the wrapped key behind its PRF
 * passkey instead of an OS item, and the sealed bytes are the same shape.
 *
 * Nothing here touches storage or the network; the enrolment lifecycle that
 * does lives in `vault-enrolment.ts`.
 */

import { fromBase64, openText, randomBytes, sealText, toBase64 } from './seal';

/** The purpose the vault enrols under. */
export const VAULT_PURPOSE = 'vault';

/**
 * What the vault's material holds: what a start needs to reach the unlocked
 * session without the master password. Both are secrets.
 */
export interface VaultMaterial {
  /** The token the core redeems for an access token on a later start. */
  refresh_token: string;
  /** The account's user key, base64. */
  user_key: string;
}

/** A random 256-bit wrapping key, base64, for the gate to hold. */
export function newWrappingKey(): string | null {
  const key = randomBytes(32);
  return key === null ? null : toBase64(key);
}

/**
 * Seal the material under `key`: base64 of `iv || ciphertext`, AES-256-GCM.
 * `null` when this webview has no WebCrypto at all.
 */
export async function sealVaultMaterial(
  material: VaultMaterial,
  key: string,
): Promise<string | null> {
  const raw = fromBase64(key);
  if (raw === null) return null;
  return sealText(encodeVaultMaterial(material), raw);
}

/**
 * The material back, or `null` when the sealed bytes are not ours or the key
 * is wrong - the answer a client falls back to the password on.
 */
export async function openVaultMaterial(
  sealed: string,
  key: string,
): Promise<VaultMaterial | null> {
  const raw = fromBase64(key);
  if (raw === null) return null;
  const text = await openText(sealed, raw);
  return text === null ? null : decodeVaultMaterial(text);
}

/** The material as the JSON the sealed bytes carry. */
export function encodeVaultMaterial(material: VaultMaterial): string {
  return JSON.stringify({ refresh_token: material.refresh_token, user_key: material.user_key });
}

/**
 * The material back, or `null` when the bytes are not this shape - a stored
 * blob this build cannot read falls back to the password rather than sending
 * nonsense to the core.
 */
export function decodeVaultMaterial(blob: string): VaultMaterial | null {
  let held: unknown;
  try {
    held = JSON.parse(blob);
  } catch {
    return null;
  }
  if (held === null || typeof held !== 'object') return null;
  const material = held as Record<string, unknown>;
  const refresh = material['refresh_token'];
  const userKey = material['user_key'];
  if (typeof refresh !== 'string' || refresh === '') return null;
  if (typeof userKey !== 'string' || userKey === '') return null;
  return { refresh_token: refresh, user_key: userKey };
}

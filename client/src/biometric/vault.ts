/**
 * The vault's slice of the biometric proxy: the purpose it enrols under and
 * the blob shape it wraps.
 *
 * **The gate never looks inside a blob**, so this shape is the consumer's own
 * and nothing outside the vault reads it. A second consumer adds its own file
 * like this one and registers a purpose; the gate and its ceremony do not
 * change.
 */

/** The purpose the vault enrols under. */
export const VAULT_PURPOSE = 'vault';

/**
 * What the vault's blob holds: what a start needs to reach the unlocked
 * session without the master password. Both are secrets, and the blob is
 * wrapped by the platform authenticator, never by this module.
 */
export interface VaultMaterial {
  /** The token the core redeems for an access token on a later start. */
  refresh_token: string;
  /** The account's user key, base64. */
  user_key: string;
}

/** The blob, as the gate stores and releases it. */
export function encodeVaultMaterial(material: VaultMaterial): string {
  return JSON.stringify({ refresh_token: material.refresh_token, user_key: material.user_key });
}

/**
 * The material back, or `null` when the blob is not this shape - a stored blob
 * this build cannot read falls back to the password rather than sending
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

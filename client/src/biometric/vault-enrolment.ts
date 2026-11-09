/**
 * The vault's enrolment lifecycle: enrol this device from the material the
 * core hands out while its session is unlocked, release that material through
 * the gate on a later start, and rewrite the sealed bytes when the core
 * rotates the refresh token.
 *
 * **The gate holds the wrapping key; the core holds the sealed material.**
 * This module is the only place that knows how the two fit together, and the
 * only vault code that touches the gate at all - a second consumer writes its
 * own file with its own material and its own purpose.
 */

import type { Connection } from '../socket';
import { release, store } from './index';

/** The connection half the commands need: one dispatch doorway. */
export type VaultConnection = Pick<Connection, 'dispatch'>;
/** The connection half the rotation watcher needs: every inbound frame, and
 *  the commands a rotation's rewrite sends. */
export type VaultWatch = Pick<Connection, 'onMessage' | 'dispatch'>;
import {
  VAULT_PURPOSE,
  newWrappingKey,
  openVaultMaterial,
  sealVaultMaterial,
  type VaultMaterial,
} from './vault';

/** Where this client keeps the enrolment id it minted for this device. */
const ENROLMENT_STORAGE = 'forge.biometric.enrolment.vault';

/** The material and wrapping key this app run last held. */
interface Held {
  material: VaultMaterial;
  key: string;
}

let held: Held | null = null;

/** What a later start's release ended as, as the unlock card draws it. */
export type VaultRelease =
  | { outcome: 'released'; material: VaultMaterial }
  | { outcome: 'refused'; reason: string }
  | { outcome: 'not_enrolled' };

/** Forget the material this run held: the vault locked, or the card settled. */
export function forgetVaultMaterial(): void {
  held = null;
}

/**
 * Enrol this device, from the material the core hands out while its session
 * is unlocked. Called on the core's own `unlocked` frame, so the first
 * password login enrols and so does every later unlock that finds no current
 * blob.
 *
 * A locked session answers no material and nothing is enrolled; a build with
 * no gate (or no WebCrypto) stays on the password path silently.
 */
export async function enrolVault(connection: VaultConnection): Promise<void> {
  try {
    const material = await materialFrom(connection);
    if (material === null) return;
    const key = newWrappingKey();
    if (key === null) return;
    const sealed = await sealVaultMaterial(material, key);
    if (sealed === null) return;
    // **The gate first**: a blob registered before its key would be a record
    // no release could open.
    await store(VAULT_PURPOSE, key);
    held = { material, key };
    await writeEnrolment(connection, sealed);
  } catch {
    // A gate that cannot hold a key, or a browser that cannot make one,
    // leaves this device on the password path. The card asks for the
    // gesture only where `enrolled` answers yes, so nothing is promised.
  }
}

/**
 * Release the vault's material: the gate for the wrapping key, the core for
 * the sealed bytes, and this module for the join. Every ending is the card's
 * to draw.
 */
export async function releaseVaultMaterial(connection: VaultConnection): Promise<VaultRelease> {
  const released = await release(VAULT_PURPOSE);
  if (released.outcome !== 'released') return released;
  const enrolment = enrolmentId();
  if (enrolment === null) return { outcome: 'not_enrolled' };
  const sealed = await sealedFrom(connection, enrolment);
  if (sealed === null) {
    return { outcome: 'refused', reason: 'the core holds no enrolment for this device' };
  }
  const material = await openVaultMaterial(sealed, released.blob);
  if (material === null) {
    return {
      outcome: 'refused',
      reason: 'the stored unlock could not be opened on this device; use the password',
    };
  }
  held = { material, key: released.blob };
  return { outcome: 'released', material };
}

/**
 * Rewrite the sealed bytes when the core rotates the refresh token, and lock
 * the vault when the rewrite cannot land - decision 15's own sentence.
 *
 * **A client holding no material cannot rewrite**: it registered the sealed
 * bytes in another run, so this one lets the blob go stale and the next start
 * falls back to the password. That is the decision's other ending, not a
 * silent success.
 */
export function watchVaultEnrolment(connection: VaultWatch): () => void {
  return connection.onMessage((message) => {
    if (message.kind !== 'update') return;
    const update = message.update;
    if (typeof update === 'string') return;
    const rotated = update['vault_rotated'];
    if (rotated !== null && typeof rotated === 'object') {
      const token = (rotated as { refresh_token?: unknown }).refresh_token;
      if (typeof token === 'string' && token !== '') {
        void rewrite(connection, token);
      }
      return;
    }
    const lockChanged = update['vault_lock_changed'];
    if (lockChanged !== null && typeof lockChanged === 'object') {
      if ((lockChanged as { locked?: unknown }).locked === false) {
        // An unlock landed: the session's material is current again, so the
        // blob is refreshed from it (the first password login's enrolment is
        // this same path).
        void enrolVault(connection);
      }
      return;
    }
  });
}

/** The core's current material, or null while the session is locked. */
async function materialFrom(connection: VaultConnection): Promise<VaultMaterial | null> {
  const answer = await connection.dispatch({ vault_enrolment_material: {} });
  return materialOf(answer);
}

function materialOf(answer: unknown): VaultMaterial | null {
  if (answer === null || typeof answer !== 'object') return null;
  const heldMaterial = answer as Record<string, unknown>;
  const refresh = heldMaterial['refresh_token'];
  const userKey = heldMaterial['user_key'];
  if (typeof refresh !== 'string' || refresh === '') return null;
  if (typeof userKey !== 'string' || userKey === '') return null;
  return { refresh_token: refresh, user_key: userKey };
}

/** The sealed bytes the core stores for this device, or null. */
async function sealedFrom(connection: VaultConnection, enrolment: string): Promise<string | null> {
  const answer = await connection.dispatch({
    biometric_release: { enrolment, purpose: VAULT_PURPOSE },
  });
  if (answer === null || typeof answer !== 'object') return null;
  const blob = (answer as { blob?: unknown }).blob;
  return typeof blob === 'string' && blob !== '' ? blob : null;
}

async function rewrite(connection: VaultConnection, refreshToken: string): Promise<void> {
  const current = held;
  if (current === null) return;
  const material = { ...current.material, refresh_token: refreshToken };
  let written = false;
  try {
    const sealed = await sealVaultMaterial(material, current.key);
    if (sealed !== null) {
      held = { material, key: current.key };
      written = await writeEnrolment(connection, sealed);
    }
  } catch {
    written = false;
  }
  // A rewrite that could not land leaves the blob stale; the session locks
  // rather than let the next start discover it (decision 15).
  if (!written) await connection.dispatch('vault_lock');
}

/** Register the sealed bytes on the core; `false` when the write did not land. */
async function writeEnrolment(connection: VaultConnection, blob: string): Promise<boolean> {
  const enrolment = enrolmentId();
  if (enrolment === null) return false;
  const answer = await connection.dispatch({
    biometric_enrol: {
      enrolment,
      device: deviceName(),
      purpose: VAULT_PURPOSE,
      blob,
    },
  });
  return answer === true;
}

/**
 * The enrolment id this client minted, or null when this page cannot keep
 * one - storage that is refused loses the id across a restart, so the release
 * path falls back to the password rather than naming a device it cannot.
 */
function enrolmentId(): string | null {
  try {
    const stored = globalThis.localStorage.getItem(ENROLMENT_STORAGE);
    if (stored !== null && stored !== '') return stored;
    const minted = mintedId();
    if (minted === null) return null;
    globalThis.localStorage.setItem(ENROLMENT_STORAGE, minted);
    return minted;
  } catch {
    return null;
  }
}

/** A UUID v4 from the platform's random source, or null without one. */
function mintedId(): string | null {
  const crypto = globalThis.crypto;
  if (crypto === undefined) return null;
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x40;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * The device's name for the settings list. **The platform, not the model**:
 * the page has no hostname on any of the three clients, so two devices of one
 * platform are told apart by their enrolment stamps rather than their names.
 */
export function deviceName(): string {
  const data = (globalThis.navigator as { userAgentData?: { platform?: string } }).userAgentData;
  const platform = data?.platform ?? globalThis.navigator.platform;
  return platform === undefined || platform === '' ? 'this device' : platform;
}

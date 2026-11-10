/**
 * The vault's enrolment lifecycle: enrol this device when the person asks,
 * release that material through the gate on a later start, and rewrite the
 * sealed bytes when the core rotates the refresh token.
 *
 * **Enrolment is never automatic.** A `vault_lock_changed` frame is slot-less
 * and reaches every connected client, so enrolling on it made every connected
 * client a permanent device and let a revoked phone re-register itself on the
 * next unlock. The settings card's enrol control is the only caller, and it
 * needs the session unlocked - which is the password - so a revocation sticks
 * until the person enrols that device again.
 *
 * **The gate holds the wrapping key; the core holds the sealed material.**
 * This module is the only place that knows how the two fit together, and the
 * only vault code that touches the gate at all - a second consumer writes its
 * own file with its own material and its own purpose.
 */

import type { Connection } from '../socket';
import { reasonOf, release, remove, store } from './index';

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

/** What one enrol attempt ended as, as the settings card draws it. */
export type VaultEnrolment =
  | { outcome: 'enrolled' }
  | { outcome: 'needs_unlock' }
  | { outcome: 'unavailable'; reason: string }
  | { outcome: 'failed'; reason: string };

/** Forget the material this run held: the vault locked, or the card settled. */
export function forgetVaultMaterial(): void {
  held = null;
}

/**
 * Enrol this device, from the material the core hands out while its session
 * is unlocked. Called by the settings card's enrol control and nothing else:
 * the person's own action, taken while the vault is open.
 *
 * A locked session answers no material and nothing is enrolled; a device
 * whose gate cannot hold a key says why, so the card can tell the person
 * instead of leaving the password path unexplained.
 */
export async function enrolVault(connection: VaultConnection): Promise<VaultEnrolment> {
  let material: VaultMaterial | null;
  try {
    material = await materialFrom(connection);
  } catch (why) {
    return { outcome: 'failed', reason: reasonOf(why) };
  }
  if (material === null) return { outcome: 'needs_unlock' };
  const key = newWrappingKey();
  if (key === null) {
    return { outcome: 'unavailable', reason: 'this browser cannot make a wrapping key' };
  }
  const sealed = await sealVaultMaterial(material, key);
  if (sealed === null) return { outcome: 'failed', reason: 'the material could not be sealed' };
  // **The gate first**: a blob registered before its key would be a record
  // no release could open.
  try {
    await store(VAULT_PURPOSE, key);
  } catch (why) {
    // **A refused prompt is an attempt that failed**, not a gate that cannot
    // hold a key: the press stays so the person can try again.
    return { outcome: 'failed', reason: reasonOf(why) };
  }
  let written: boolean;
  try {
    written = await writeEnrolment(connection, sealed);
  } catch (why) {
    return { outcome: 'failed', reason: reasonOf(why) };
  }
  if (!written) return { outcome: 'failed', reason: 'the core did not store the enrolment' };
  // **Held only once the write landed.** Material kept past a refused write
  // would let a later rotation complete an enrolment the card reported
  // failed.
  held = { material, key };
  return { outcome: 'enrolled' };
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
    // **The core holds nothing for this device.** The item the gate just
    // opened is a registration the person already revoked, so it goes too
    // rather than leaving a gesture that can never succeed, and so does the
    // material this run held - a later rotation must not rewrite a row the
    // person deleted.
    void remove(VAULT_PURPOSE);
    forgetVaultMaterial();
    return { outcome: 'not_enrolled' };
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
 * Keep the sealed bytes current while the session is open: rewrite them when
 * the core rotates the refresh token, and lock the vault when the rewrite
 * cannot land - decision 15's own sentence.
 *
 * **A lock drops the material this run held**, so nothing outlives the
 * session it was unlocked for. **A client holding no material cannot
 * rewrite**: it registered the sealed bytes in another run, so this one lets
 * the blob go stale and the next start falls back to the password. That is
 * the decision's other ending, not a silent success.
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
    const changed = update['biometric_enrolments_changed'];
    if (changed !== null && typeof changed === 'object') {
      // **A row this device no longer owns is a revocation.** Dropping the
      // material here is what stops the next rotation from rewriting the row
      // the person just deleted; the rewrite itself is update-only, so a
      // disconnected device still cannot re-create one.
      if (!stillEnrolled((changed as { enrolments?: unknown }).enrolments)) {
        forgetVaultMaterial();
      }
      return;
    }
    const lockChanged = update['vault_lock_changed'];
    if (lockChanged !== null && typeof lockChanged === 'object') {
      if ((lockChanged as { locked?: unknown }).locked === true) forgetVaultMaterial();
      return;
    }
  });
}

/** Whether the announced list still carries this device's enrolment id. */
function stillEnrolled(enrolments: unknown): boolean {
  const mine = enrolmentIdIfMinted();
  if (mine === null || !Array.isArray(enrolments)) return false;
  return enrolments.some(
    (entry) =>
      entry !== null && typeof entry === 'object' && (entry as { id?: unknown }).id === mine,
  );
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
    if (sealed !== null) written = await writeRewrite(connection, sealed);
  } catch {
    written = false;
  }
  if (written) {
    held = { material, key: current.key };
    return;
  }
  // A rewrite that could not land leaves the blob stale; the session locks
  // rather than let the next start discover it (decision 15).
  await connection.dispatch('vault_lock');
}

/** Register the sealed bytes on the core; `false` when the write did not land. */
async function writeEnrolment(connection: VaultConnection, blob: string): Promise<boolean> {
  const enrolment = enrolmentId();
  if (enrolment === null) return false;
  const answer = await connection.dispatch({
    biometric_enrol: {
      enrolment,
      device: deviceName(enrolment),
      purpose: VAULT_PURPOSE,
      blob,
    },
  });
  return answer === true;
}

/**
 * Replace the sealed bytes of an enrolment the core already holds.
 *
 * **This cannot create**: the server answers `false` when the id or the
 * purpose is gone, which is exactly the revoked device's case, and the caller
 * locks instead of re-registering.
 */
async function writeRewrite(connection: VaultConnection, blob: string): Promise<boolean> {
  const enrolment = enrolmentIdIfMinted();
  if (enrolment === null) return false;
  const answer = await connection.dispatch({
    biometric_rewrite: { enrolment, purpose: VAULT_PURPOSE, blob },
  });
  return answer === true;
}

/**
 * The enrolment id this client minted, or null when this page cannot keep
 * one - storage that is refused loses the id across a restart, so the release
 * path falls back to the password rather than naming a device it cannot.
 */
export function enrolmentId(): string | null {
  try {
    const stored = enrolmentIdIfMinted();
    if (stored !== null) return stored;
    const minted = mintedId();
    if (minted === null) return null;
    globalThis.localStorage.setItem(ENROLMENT_STORAGE, minted);
    return minted;
  } catch {
    return null;
  }
}

/**
 * The id this client already minted, or null - **a read, not a mint**. A
 * comparison against the server's list must not create an id as a side
 * effect; only enrolling does that.
 */
export function enrolmentIdIfMinted(): string | null {
  try {
    const stored = globalThis.localStorage.getItem(ENROLMENT_STORAGE);
    return stored !== null && stored !== '' ? stored : null;
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
 * The device's name for the settings list. **The platform plus this
 * enrolment's own serial**: the page has no hostname on any of the three
 * clients, so the id the client minted is what tells two devices of one
 * platform apart.
 */
export function deviceName(enrolment: string): string {
  const data = (globalThis.navigator as { userAgentData?: { platform?: string } }).userAgentData;
  const platform = data?.platform ?? globalThis.navigator.platform;
  const name = platform === undefined || platform === '' ? 'this device' : platform;
  return `${name} (${enrolment.slice(0, 8)})`;
}

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
import { deviceName } from './name';

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

/**
 * Where this client keeps the id it minted for this device.
 *
 * **One id names a device, not a purpose.** A second purpose on one device is
 * the same device, so the id lives under a key of its own rather than one per
 * purpose; every release and every row's revoke name it.
 */
const DEVICE_STORAGE = 'forge.biometric.device';

/**
 * The purpose-scoped key shipped before this cut. A client enrolled under it
 * adopts its id rather than reading "no row" and enrolling a second one.
 */
const LEGACY_DEVICE_STORAGE = 'forge.biometric.enrolment.vault';

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

/** What the settings page's direct unlock ended as. */
export type VaultUnlock = { outcome: 'unlocked' } | { outcome: 'refused'; reason: string };

/**
 * Unlock the shared vault from the settings page: the master password with
 * the optional 2FA code, or the material the gate released.
 *
 * **The command's wire literal lives here**, with the enrolment's other
 * hand-written commands, so a rename and the server's decoder move together
 * in review. A failure is drawn in the dialog, never thrown.
 */
export async function unlockVault(
  connection: VaultConnection,
  credential: { password: string; two_factor: string } | { released: VaultMaterial },
): Promise<VaultUnlock> {
  const command =
    'released' in credential
      ? { vault_unlock: { released: credential.released } }
      : {
          vault_unlock: {
            password: credential.password,
            ...(credential.two_factor === '' ? {} : { two_factor: credential.two_factor }),
          },
        };
  return unlockAnswer(await connection.dispatch(command));
}

function unlockAnswer(answer: unknown): VaultUnlock {
  if (answer === null || typeof answer !== 'object') {
    return { outcome: 'refused', reason: 'the unlock answer did not read' };
  }
  const held = answer as Record<string, unknown>;
  if (held['outcome'] === 'unlocked') return { outcome: 'unlocked' };
  const reason = held['reason'];
  return {
    outcome: 'refused',
    reason: typeof reason === 'string' && reason !== '' ? reason : 'the vault refused the unlock',
  };
}

/** Forget the material this run held: the vault locked, or the card settled. */
export function forgetVaultMaterial(): void {
  held = null;
}

/**
 * Drop the gate's record without letting a failed drop outlive the answer.
 *
 * **Every failed enrol ends here**, so "nothing was stored" is true whether
 * the core refused the write or the socket threw while it was in flight: a
 * record with no row is an orphan the next release would spend a gesture on.
 */
function dropRecord(): Promise<void> {
  return remove(VAULT_PURPOSE).catch(() => {});
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
    // **A failed store drops the record.** A passkey the person deleted in
    // the browser's own manager makes the held credential's assertion reject;
    // without the drop, every later press would fail on that same dead
    // credential. A cancelled prompt is indistinguishable from it here, so a
    // cancel costs the record and the next press mints a fresh passkey - the
    // accepted trade.
    await dropRecord();
    return { outcome: 'failed', reason: reasonOf(why) };
  }
  let written: boolean;
  try {
    written = await writeEnrolment(connection, sealed);
  } catch (why) {
    // **The answer that failed and the record go together**, on a throw as
    // much as on a refusal: a socket that closed mid-write leaves no row.
    await dropRecord();
    return { outcome: 'failed', reason: reasonOf(why) };
  }
  if (!written) {
    await dropRecord();
    return { outcome: 'failed', reason: 'the core did not store the enrolment' };
  }
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
  // **No id is no row, and a release never mints one.** A device that has
  // never enrolled has nothing for a gesture to open, so the prompt is not
  // raised and the card draws the password instead.
  const enrolment = enrolmentIdIfMinted();
  if (enrolment === null) return { outcome: 'not_enrolled' };
  // **The row before the gesture.** A device revoked elsewhere holds nothing,
  // so a passkey prompt would be spent on a call that cannot land; the core's
  // answer is read first and a missing row is answered without one.
  const sealed = await sealedFrom(connection, enrolment);
  if (sealed === null) {
    // **The core holds nothing for this device.** The item is a registration
    // the person already revoked, so it goes too rather than leaving a
    // gesture that can never succeed, and so does the material this run held -
    // a later rotation must not rewrite a row the person deleted.
    await dropRecord();
    forgetVaultMaterial();
    return { outcome: 'not_enrolled' };
  }
  const released = await release(VAULT_PURPOSE);
  if (released.outcome !== 'released') return released;
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
 * The device id this client minted, minting one when there is none, or null
 * when this page cannot keep one - storage that is refused loses the id
 * across a restart, so the release path falls back to the password rather
 * than naming a device it cannot.
 */
export function enrolmentId(): string | null {
  try {
    const stored = enrolmentIdIfMinted();
    if (stored !== null) return stored;
    const minted = mintedId();
    if (minted === null) return null;
    globalThis.localStorage.setItem(DEVICE_STORAGE, minted);
    return minted;
  } catch {
    return null;
  }
}

/**
 * The id this client already minted, or null - **a read, not a mint**. A
 * comparison against the server's list must not create an id as a side
 * effect; only enrolling does that.
 *
 * **An id under the pre-rename key is adopted**, not read as absent: a client
 * enrolled before the device-scoped key would otherwise see "the server has
 * no row" and enrol a second one. Adoption moves the same UUID to the new key
 * and drops the old one.
 */
export function enrolmentIdIfMinted(): string | null {
  try {
    const stored = globalThis.localStorage.getItem(DEVICE_STORAGE);
    if (stored !== null && stored !== '') return stored;
    const legacy = globalThis.localStorage.getItem(LEGACY_DEVICE_STORAGE);
    if (legacy === null || legacy === '') return null;
    globalThis.localStorage.setItem(DEVICE_STORAGE, legacy);
    globalThis.localStorage.removeItem(LEGACY_DEVICE_STORAGE);
    return legacy;
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

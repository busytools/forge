/**
 * This device's own gate state for the vault, as the settings card draws it.
 *
 * **The core's device list is not this.** That list is every device the
 * server holds; this is whether *this* client can answer the gesture, which
 * is a local question - the record an enrol minted on this machine - and the
 * answer the enrol control needs.
 *
 * **The keychain is not asked.** On macOS a read that decides whether an item
 * exists is itself a Touch ID prompt, and a skipped read of a protected item
 * answers not-found, so the keychain can neither prompt nor lie for this
 * question. The record is also what the release names: a client whose id is
 * gone cannot release, so "ready to enrol" is the truthful state.
 */

import { availability } from './index';
import { enrolmentIdIfMinted } from './vault-enrolment';

/** A gate that can hold a key, and which gesture answers it. `biometric` is
 *  false on a Mac with no sensor: the device password is the gesture then,
 *  and the dialog's button names it. */
type Answering = { biometric: boolean };

export type DeviceGate =
  | { outcome: 'reading' }
  | ({ outcome: 'enrolled' } & Answering)
  | ({ outcome: 'ready' } & Answering)
  | { outcome: 'enrolling' }
  | { outcome: 'unavailable'; reason: string }
  | { outcome: 'failed'; reason: string };

/** Read whether this device can enrol, and whether it already has. */
export async function readDeviceGate(): Promise<DeviceGate> {
  const gate = await availability();
  if (!gate.available) {
    return { outcome: 'unavailable', reason: gate.reason ?? 'this device has no authenticator' };
  }
  const biometric = gate.biometric;
  return enrolmentIdIfMinted() !== null
    ? { outcome: 'enrolled', biometric }
    : { outcome: 'ready', biometric };
}

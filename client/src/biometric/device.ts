/**
 * This device's own gate state for the vault, as the settings card draws it.
 *
 * **The core's device list is not this.** That list is every device the
 * server holds; this is whether *this* client can answer the gesture, which
 * is a local question - the gate's item on this machine - and the answer the
 * enrol control needs.
 */

import { availability, enrolled } from './index';
import { VAULT_PURPOSE } from './vault';

export type DeviceGate =
  | { outcome: 'reading' }
  | { outcome: 'enrolled' }
  | { outcome: 'ready' }
  | { outcome: 'enrolling' }
  | { outcome: 'needs_unlock' }
  | { outcome: 'unavailable'; reason: string }
  | { outcome: 'failed'; reason: string };

/** Read whether this device can enrol, and whether it already has. */
export async function readDeviceGate(): Promise<DeviceGate> {
  const gate = await availability();
  if (!gate.available) {
    return { outcome: 'unavailable', reason: gate.reason ?? 'this device has no authenticator' };
  }
  return (await enrolled(VAULT_PURPOSE)) ? { outcome: 'enrolled' } : { outcome: 'ready' };
}

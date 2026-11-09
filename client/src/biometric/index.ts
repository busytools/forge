/**
 * The platform authenticator, as the page uses it: a purpose's wrapped key
 * stored, released or dropped, and whether this device can answer.
 *
 * **The gate is one door with a purpose key**, which is what makes a second
 * consumer a new purpose and its own blob shape rather than a new ceremony.
 * On the desktop and the phone the shell's gate answers through `invoke`; in
 * a browser `web.ts` answers behind these same functions, its passkey record
 * taking the place of the OS item.
 */

import { invoke, isTauri } from '@tauri-apps/api/core';

import * as web from './web';

/** Whether this device can answer, and with which gesture. */
export interface Availability {
  available: boolean;
  /** True for a biometric gesture, false when the fallback is the device
   *  password (user presence). */
  biometric: boolean;
  /** Why it cannot answer, when it cannot. The card quotes it. */
  reason?: string;
}

/** What one release ended as. */
export type Release =
  | { outcome: 'released'; blob: string }
  | { outcome: 'refused'; reason: string }
  | { outcome: 'not_enrolled' };

/** Whether this device can release a purpose's blob at all. */
export async function availability(): Promise<Availability> {
  if (!isTauri()) return web.availability();
  try {
    return await invoke<Availability>('biometric_availability');
  } catch (why) {
    return { available: false, biometric: false, reason: reasonOf(why) };
  }
}

/** Whether this device holds a blob for `purpose`. */
export async function enrolled(purpose: string): Promise<boolean> {
  if (!isTauri()) return web.enrolled(purpose);
  try {
    return await invoke<boolean>('biometric_enrolled', { purpose });
  } catch {
    // A gate that cannot answer whether it holds an item is a gate that
    // cannot be offered; the caller draws the password.
    return false;
  }
}

/** Store `purpose`'s wrapped blob, replacing any blob already there. */
export async function store(purpose: string, blob: string): Promise<void> {
  if (!isTauri()) return web.store(purpose, blob);
  await invoke<void>('biometric_store', { purpose, blob });
}

/** Release `purpose`'s blob, asking the platform authenticator first. */
export async function release(purpose: string): Promise<Release> {
  try {
    if (!isTauri()) return await web.release(purpose);
    return await invoke<Release>('biometric_release', { purpose });
  } catch (why) {
    // A cancelled passkey prompt or a refused one reaches here as its own
    // error; the card draws the platform's words and keeps the password.
    return { outcome: 'refused', reason: reasonOf(why) };
  }
}

/** Drop `purpose`'s blob. */
export async function remove(purpose: string): Promise<void> {
  if (!isTauri()) {
    web.remove(purpose);
    return;
  }
  await invoke<void>('biometric_remove', { purpose });
}

/**
 * The platform's own words, which the card quotes. A rejection from the shell
 * is a string; anything else is not worded for a reader and gets a plain one.
 */
export function reasonOf(why: unknown): string {
  if (typeof why === 'string' && why !== '') return why;
  if (why instanceof Error && why.message !== '') return why.message;
  return 'the platform authenticator did not answer';
}

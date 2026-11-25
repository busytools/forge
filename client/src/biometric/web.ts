/**
 * The browser gate: a WebAuthn platform credential created with the PRF
 * extension, whose PRF output wraps the purpose's key.
 *
 * **There is no bare-biometric web API.** The passkey is the credential and
 * Touch ID or the phone's sensor is the gesture the browser asks for. The PRF
 * output is deterministic per credential and salt, so it derives the same
 * wrapping key on every assertion; the wrapped purpose key lives in
 * localStorage and is useless without a fresh gesture. Where a browser has no
 * PRF, creation says so plainly and the password path stands.
 *
 * The record is per purpose, exactly as the desktop items are: a second
 * consumer registers its own passkey and wraps its own key, and nothing here
 * learns what a purpose means.
 */

import type { Availability, Release } from './index';
import { fresh, fromBase64, openText, randomBytes, sealText, toBase64 } from './seal';

/** The localStorage prefix a purpose's record lives under. */
const RECORD_PREFIX = 'forge.biometric.web.';

/** What one purpose's local record holds. */
interface WebRecord {
  /** The credential's raw id, base64. */
  credential: string;
  /** The purpose key, sealed under the PRF output. */
  wrapped: string;
}

/** Whether this browser can answer at all, and with what gesture. */
export async function availability(): Promise<Availability> {
  // **The secure context is checked first.** An insecure page has no WebAuthn
  // at all, and reporting that as "this browser has no WebAuthn" sends a
  // reader looking at the wrong thing.
  if ((globalThis as { isSecureContext?: boolean }).isSecureContext === false) {
    return {
      available: false,
      biometric: false,
      reason:
        'this page is not a secure origin (https or localhost), so the browser offers no passkey; use the password',
    };
  }
  const platform = globalThis.PublicKeyCredential;
  if (platform === undefined) {
    return {
      available: false,
      biometric: false,
      reason: 'this browser has no WebAuthn, so there is no gesture to offer; use the password',
    };
  }
  if (globalThis.crypto?.subtle === undefined) {
    return {
      available: false,
      biometric: false,
      reason:
        'this browser has no WebCrypto, so a passkey could not seal anything; use the password',
    };
  }
  try {
    const usable = await platform.isUserVerifyingPlatformAuthenticatorAvailable();
    if (!usable) {
      return {
        available: false,
        biometric: false,
        reason: 'this browser has no platform authenticator, so there is no gesture to offer',
      };
    }
  } catch {
    return {
      available: false,
      biometric: false,
      reason: 'the browser could not check for a platform authenticator',
    };
  }
  if ((await prfSupport(platform)) === 'unsupported') {
    return {
      available: false,
      biometric: false,
      reason:
        'this browser supports no passkey PRF, so a passkey cannot keep the unlock; use the password',
    };
  }
  return { available: true, biometric: true };
}

/** What the browser says about the PRF extension, before any credential exists. */
type PrfSupport = 'supported' | 'unsupported' | 'unknown';

/**
 * What the browser reports for `extension:prf`.
 *
 * **An absent answer is `unknown`, not `unsupported`.** A browser with no
 * capability API, or one that omits the key, may still answer PRF at
 * creation; the enrol attempt is what settles it, and refusing the press on
 * absence would close a path that works.
 */
async function prfSupport(platform: typeof globalThis.PublicKeyCredential): Promise<PrfSupport> {
  const read = (
    platform as unknown as {
      getClientCapabilities?: () => Promise<Record<string, boolean>>;
    }
  ).getClientCapabilities;
  if (read === undefined) return 'unknown';
  try {
    const capabilities = await read.call(platform);
    const supported = capabilities['extension:prf'];
    if (supported === true) return 'supported';
    if (supported === false) return 'unsupported';
    return 'unknown';
  } catch {
    return 'unknown';
  }
}

/** Whether this browser holds a passkey record for `purpose`. */
export function enrolled(purpose: string): boolean {
  return readRecord(purpose) !== null;
}

/**
 * Seal `key` under a passkey's PRF output, reusing this purpose's passkey when
 * it still works.
 *
 * **A re-enrol replaces the sealed key, not the passkey.** The press is the
 * route back from a stale blob, and one press must not leave another passkey
 * in the person's own passkey manager. A record the passkey cannot open is
 * dead - the PRF is deterministic - so it is dropped and a fresh passkey
 * replaces it.
 */
export async function store(purpose: string, key: string): Promise<void> {
  const existing = readRecord(purpose);
  const credential = existing === null ? null : fromBase64(existing.credential);
  if (existing !== null && credential !== null) {
    const prf = await assertPasskey(purpose, credential);
    if (prf !== null && (await openText(existing.wrapped, prf)) !== null) {
      const wrapped = await sealText(key, prf);
      if (wrapped === null) {
        throw new Error('the passkey released no usable key, so nothing was stored');
      }
      writeRecord(purpose, { credential: existing.credential, wrapped });
      return;
    }
    // **The record is dead**, so it goes rather than leaving a gesture that
    // can never succeed; the fresh passkey below is the replacement.
    remove(purpose);
  } else if (existing !== null) {
    remove(purpose);
  }
  const created = await createPasskey(purpose);
  const wrapped = await sealText(key, created.prf);
  if (wrapped === null) {
    throw new Error('the passkey released no usable key, so nothing was stored');
  }
  writeRecord(purpose, { credential: created.credential, wrapped });
}

/** Ask the passkey for the gesture and unwrap the purpose key. */
export async function release(purpose: string): Promise<Release> {
  const record = readRecord(purpose);
  if (record === null) return { outcome: 'not_enrolled' };
  const credential = fromBase64(record.credential);
  if (credential === null) return { outcome: 'not_enrolled' };
  const prf = await assertPasskey(purpose, credential);
  if (prf === null) {
    return {
      outcome: 'refused',
      reason: 'the passkey released no key; use the password',
    };
  }
  const key = await openText(record.wrapped, prf);
  if (key === null) {
    // **A record this passkey cannot open is dead.** The PRF is deterministic,
    // so the next attempt would fail the same way; dropping the record sends
    // the next unlock straight to the password instead of another gesture.
    remove(purpose);
    return {
      outcome: 'refused',
      reason: 'the stored unlock could not be opened with this passkey; use the password',
    };
  }
  return { outcome: 'released', blob: key };
}

/**
 * Drop the purpose's local record. **The platform credential itself stays**:
 * a browser cannot delete a passkey from the page, and a record nothing points
 * at is inert.
 */
export function remove(purpose: string): void {
  try {
    globalThis.localStorage.removeItem(RECORD_PREFIX + purpose);
  } catch {
    // Storage that refuses the delete leaves the record; the next enrol
    // overwrites it, so there is nothing this call can do.
  }
}

async function createPasskey(
  purpose: string,
): Promise<{ credential: string; prf: Uint8Array<ArrayBuffer> }> {
  const challenge = randomBytes(32);
  const userId = randomBytes(16);
  if (challenge === null || userId === null) {
    throw new Error('this browser has no random source');
  }
  // The PRF extension is not in the DOM's types yet; the cast is the boundary
  // where that untyped input enters, and the output is narrowed again below.
  const options = {
    publicKey: {
      challenge,
      rp: { name: 'forge' },
      user: { id: userId, name: 'forge', displayName: 'forge' },
      pubKeyCredParams: [
        { type: 'public-key', alg: -7 },
        { type: 'public-key', alg: -257 },
      ],
      authenticatorSelection: {
        authenticatorAttachment: 'platform',
        userVerification: 'required',
        residentKey: 'required',
      },
      timeout: 60_000,
      attestation: 'none',
      extensions: { prf: { eval: { first: prfSalt(purpose) } } },
    },
  } as unknown as CredentialCreationOptions;
  const created = (await globalThis.navigator.credentials.create(
    options,
  )) as PublicKeyCredential | null;
  if (created === null) throw new Error('the browser created no passkey');
  const credential = toBase64(new Uint8Array(created.rawId));
  const prf = prfOutput(created);
  if (prf !== null) return { credential, prf };
  // **Created but withheld.** Some authenticators report PRF enabled without
  // returning the output at creation; one assertion fetches it, so a working
  // authenticator is not read as PRF-less.
  if (prfEnabled(created) === true) {
    const asserted = await assertPasskey(purpose, new Uint8Array(created.rawId));
    if (asserted !== null) return { credential, prf: asserted };
  }
  throw new Error(
    'this browser does not support the PRF extension, so nothing was stored; use the password',
  );
}

/** Whether a created credential reports the PRF extension enabled. */
function prfEnabled(credential: PublicKeyCredential): boolean | undefined {
  const results = credential.getClientExtensionResults() as { prf?: { enabled?: boolean } };
  return results.prf?.enabled;
}

async function assertPasskey(
  purpose: string,
  credential: Uint8Array<ArrayBuffer>,
): Promise<Uint8Array<ArrayBuffer> | null> {
  const challenge = randomBytes(32);
  if (challenge === null) return null;
  const options = {
    publicKey: {
      challenge,
      allowCredentials: [{ type: 'public-key', id: credential }],
      userVerification: 'required',
      timeout: 60_000,
      extensions: { prf: { eval: { first: prfSalt(purpose) } } },
    },
  } as unknown as CredentialRequestOptions;
  const asserted = (await globalThis.navigator.credentials.get(
    options,
  )) as PublicKeyCredential | null;
  return asserted === null ? null : prfOutput(asserted);
}

/** The PRF output a created or asserted credential carries, when it carries one. */
function prfOutput(credential: PublicKeyCredential): Uint8Array<ArrayBuffer> | null {
  const results = credential.getClientExtensionResults() as {
    prf?: { results?: { first?: ArrayBuffer | null } };
  };
  const first = results.prf?.results?.first;
  if (first === undefined || first === null) return null;
  const bytes = fresh(first.byteLength);
  bytes.set(new Uint8Array(first));
  return bytes;
}

/** The same salt on every call: the passkey derives one key per purpose. */
function prfSalt(purpose: string): Uint8Array<ArrayBuffer> {
  const text = new TextEncoder().encode(`forge.biometric.prf.${purpose}`);
  const salt = fresh(text.length);
  salt.set(text);
  return salt;
}

function recordName(purpose: string): string {
  return RECORD_PREFIX + purpose;
}

function readRecord(purpose: string): WebRecord | null {
  try {
    const raw = globalThis.localStorage.getItem(recordName(purpose));
    if (raw === null) return null;
    const held = JSON.parse(raw) as Record<string, unknown>;
    const credential = held['credential'];
    const wrapped = held['wrapped'];
    if (typeof credential !== 'string' || credential === '') return null;
    if (typeof wrapped !== 'string' || wrapped === '') return null;
    return { credential, wrapped };
  } catch {
    return null;
  }
}

function writeRecord(purpose: string, record: WebRecord): void {
  globalThis.localStorage.setItem(recordName(purpose), JSON.stringify(record));
}

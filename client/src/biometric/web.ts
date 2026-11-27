/**
 * The browser gate: a WebAuthn platform credential created with the PRF
 * extension, whose PRF output wraps the purpose's key.
 *
 * **There is no bare-biometric web API.** The passkey is the credential and
 * Touch ID or the phone's sensor is the gesture the browser asks for. The PRF
 * output is deterministic per credential and salt, so it derives the same
 * wrapping key on every assertion; the wrapped purpose key lives in
 * localStorage and is useless without a fresh gesture. A passkey that cannot
 * unlock - a manager answered without PRF - is named as that, with the route
 * to remove it and the password as the other way in.
 *
 * The record is per purpose, exactly as the desktop items are: a second
 * consumer wraps its own key under its own salt, and nothing here learns what
 * a purpose means. A discoverable scan may be answered by a passkey another
 * purpose already holds; the salt still derives this purpose's key.
 */

import type { Availability, Release } from './index';
import { fresh, fromBase64, openText, randomBytes, sealText, toBase64 } from './seal';

/** The localStorage prefix a purpose's record lives under. */
const RECORD_PREFIX = 'forge.biometric.web.';

/** The localStorage prefix marking that this purpose minted a passkey here. */
const MINTED_PREFIX = 'forge.biometric.web.minted.';

/**
 * What a passkey that cannot unlock is told as, in the card's own line.
 *
 * **The browser does not name what answered.** Attestation is `none` and no
 * API reports which passkey provider answered, and a platform authenticator
 * with no PRF lands here as well as a manager. The words name both places a
 * passkey can be removed and keep the password as the other way in.
 */
const NO_UNLOCK_CREATED =
  "a passkey was saved that cannot unlock; remove it in your passkey manager or the browser's saved-passkeys settings, or use the password";
const NO_UNLOCK_HELD =
  "this device already holds a passkey that cannot unlock; remove it in your passkey manager or the browser's saved-passkeys settings, or use the password";

/** What a dismissed prompt is told as. It stores nothing and keeps the marker. */
const PICKER_DISMISSED =
  'the passkey prompt did not answer, so nothing was stored; press enrol again to retry, or use the password';

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
        reason:
          'this browser has no platform authenticator, so there is no gesture to offer; use the password',
      };
    }
  } catch {
    return {
      available: false,
      biometric: false,
      reason: 'the browser could not check for a platform authenticator; use the password',
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

/**
 * Seal `key` under a passkey's PRF output, reusing this purpose's passkey when
 * this browser still holds one or can find it again.
 *
 * **A re-enrol replaces the sealed key, not the passkey.** The press is the
 * route back from a stale blob, and one press must not leave another passkey
 * in the person's own passkey manager. The PRF is deterministic, so the same
 * credential reseals the new key whether or not the old bytes still open.
 * **A record this browser lost is not a passkey it lost**: a purpose that
 * minted one here before is asked for it by a discoverable assertion, so a
 * revoke, a dropped record or a stale blob reseals the same credential. A
 * fresh passkey is minted only when the browser has no marker for this
 * purpose (a cleared profile) or the scan finds nothing.
 */
export async function store(purpose: string, key: string): Promise<void> {
  const existing = readRecord(purpose);
  if (existing !== null) {
    const credential = fromBase64(existing.credential);
    if (credential !== null) {
      // **A dismissed gesture is not a dead record.** The prompt's own words
      // are not for a reader, and the held passkey is still there, so the
      // refusal is the gate's own and the press can be repeated.
      let prf: Uint8Array<ArrayBuffer> | null;
      try {
        prf = await assertPasskey(purpose, credential);
      } catch {
        throw new Error(PICKER_DISMISSED);
      }
      if (prf === null) {
        // **A held passkey that cannot unlock is reported, not replaced.**
        // The record is dead and goes, but a second credential would leave
        // the person with two they cannot use; the refusal names the one to
        // remove.
        remove(purpose);
        throw new Error(NO_UNLOCK_HELD);
      }
      const wrapped = await sealText(key, prf);
      if (wrapped === null) {
        throw new Error('the passkey released no usable key, so nothing was stored');
      }
      writeRecord(purpose, { credential: existing.credential, wrapped });
      return;
    }
    remove(purpose);
  }
  if (mintedHere(purpose)) {
    const discovered = await discoverPasskey(purpose);
    if (discovered !== null) {
      const prf = prfOutput(discovered);
      if (prf === null) {
        // **A discoverable passkey that cannot unlock is not replaced** by a
        // second one, for the same reason as the held case above.
        throw new Error(NO_UNLOCK_HELD);
      }
      const wrapped = await sealText(key, prf);
      if (wrapped === null) {
        throw new Error('the passkey released no usable key, so nothing was stored');
      }
      writeRecord(purpose, {
        credential: toBase64(new Uint8Array(discovered.rawId)),
        wrapped,
      });
      return;
    }
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
 * at is inert. **The minted marker stays too**: it is how a later enrol finds
 * that passkey rather than minting another.
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
  // **The credential exists in the person's manager from here on**, whatever
  // the PRF answers. The marker is what lets a later enrol find it again and
  // report it instead of minting a second one.
  markMinted(purpose);
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
  throw new Error(NO_UNLOCK_CREATED);
}

/**
 * A passkey this browser minted for `purpose` before, found by a discoverable
 * assertion with no `allowCredentials`, or null when none answered.
 *
 * **The scan runs only when the marker says one was minted here.** Running it
 * on a first enrol would open an empty passkey picker before every create.
 */
async function discoverPasskey(purpose: string): Promise<PublicKeyCredential | null> {
  const challenge = randomBytes(32);
  if (challenge === null) return null;
  const options = {
    publicKey: {
      challenge,
      userVerification: 'required',
      timeout: 60_000,
      extensions: { prf: { eval: { first: prfSalt(purpose) } } },
    },
  } as unknown as CredentialRequestOptions;
  try {
    return (await globalThis.navigator.credentials.get(options)) as PublicKeyCredential | null;
  } catch {
    // **A dismissed picker is not an absent passkey, and it is not a create.**
    // No credential was made this time, so the marker stays: the passkey it
    // records is still in the manager, and dropping the marker here would let
    // the next press mint a second one beside it.
    throw new Error(PICKER_DISMISSED);
  }
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

function mintedName(purpose: string): string {
  return MINTED_PREFIX + purpose;
}

/** Record that this purpose minted a passkey here, across any record loss. */
function markMinted(purpose: string): void {
  try {
    globalThis.localStorage.setItem(mintedName(purpose), '1');
  } catch {
    // Storage that refuses the flag costs the next re-enrol its scan; the
    // credential itself is already made, so there is nothing else here.
  }
}

function mintedHere(purpose: string): boolean {
  try {
    return globalThis.localStorage.getItem(mintedName(purpose)) !== null;
  } catch {
    return false;
  }
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

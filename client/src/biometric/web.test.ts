// @vitest-environment jsdom
/**
 * The browser gate, against a WebAuthn double: the passkey's PRF output wraps
 * the purpose key, a fresh assertion with the same PRF unwraps it, and a
 * browser with no PRF says so instead of pretending.
 *
 * **What a JVM test cannot reach is the platform authenticator itself** - a
 * real Touch ID or phone sensor in a PRF-capable browser - so this pins the
 * ceremony's own logic: the options it sends, the record it keeps and the
 * seal/unseal round trip. The live check needs a PRF-capable browser profile
 * and is stated as unexercised in the PR.
 */
import { webcrypto } from 'node:crypto';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import * as gate from './web';

const PRF = new Uint8Array(32).fill(7);
const OTHER_PRF = new Uint8Array(32).fill(9);

/** The credential double a create or an assert answers with. */
function aCredential(prf: Uint8Array | null, rawId = new Uint8Array([1, 2, 3, 4])): unknown {
  return {
    rawId: rawId.buffer,
    getClientExtensionResults: () =>
      prf === null ? {} : { prf: { results: { first: prf.buffer.slice(0) } } },
  };
}

let created: Record<string, unknown>[] = [];
let asserted: Record<string, unknown>[] = [];

/** The PRF salt one create or get sent, as its bytes. */
function saltOf(options: Record<string, unknown> | undefined): number[] {
  const extensions = (options?.['publicKey'] as Record<string, unknown> | undefined)?.[
    'extensions'
  ] as { prf?: { eval?: { first?: Uint8Array } } } | undefined;
  const salt = extensions?.prf?.eval?.first;
  return salt === undefined ? [] : [...salt];
}

/** The salt a purpose must use, byte for byte. */
function pinned(purpose: string): number[] {
  return [...new TextEncoder().encode(`forge.biometric.prf.${purpose}`)];
}

beforeEach(() => {
  vi.stubGlobal('crypto', webcrypto);
  vi.stubGlobal('PublicKeyCredential', {
    isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(true),
  });
  globalThis.localStorage.clear();
  created = [];
  asserted = [];
  Object.defineProperty(globalThis.navigator, 'credentials', {
    configurable: true,
    value: {
      create: (options: Record<string, unknown>) => {
        created.push(options);
        return Promise.resolve(aCredential(PRF));
      },
      get: (options: Record<string, unknown>) => {
        asserted.push(options);
        return Promise.resolve(aCredential(PRF));
      },
    },
  });
});

describe('the web gate', () => {
  it('seals the purpose key under the passkey PRF and opens it on a fresh assertion', async () => {
    await gate.store('vault', 'the-purpose-key');
    expect(gate.enrolled('vault'), "the record is this purpose's own").toBe(true);
    expect(gate.enrolled('sso')).toBe(false);
    const create = created[0]?.['publicKey'] as Record<string, unknown> | undefined;
    expect(create?.['authenticatorSelection']).toMatchObject({
      authenticatorAttachment: 'platform',
      userVerification: 'required',
    });
    expect(create?.['extensions']).toHaveProperty('prf');
    // **The salt is the purpose, pinned byte for byte**: create and get must
    // derive the same wrapping key, so a change on either side is a blob no
    // release can open.
    expect(saltOf(created[0]), 'the creation salt is the purpose').toEqual(pinned('vault'));

    const released = await gate.release('vault');

    expect(released).toEqual({ outcome: 'released', blob: 'the-purpose-key' });
    expect(saltOf(asserted[0]), 'the assertion uses the same salt').toEqual(pinned('vault'));
  });

  it('salts each purpose with its own name', async () => {
    await gate.store('vault', 'vault-key');
    await gate.store('sso', 'sso-key');

    const vault = saltOf(created[0]);
    const sso = saltOf(created[1]);
    expect(vault, 'the first purpose keeps its own salt').toEqual(pinned('vault'));
    expect(sso, 'a second consumer gets its own').toEqual(pinned('sso'));
    expect(vault).not.toEqual(sso);
    expect(gate.enrolled('vault') && gate.enrolled('sso')).toBe(true);
  });

  it('refuses when the assertion carries a different PRF', async () => {
    await gate.store('vault', 'the-purpose-key');
    Object.defineProperty(globalThis.navigator, 'credentials', {
      configurable: true,
      value: {
        create: () => Promise.resolve(aCredential(PRF)),
        get: (options: Record<string, unknown>) => {
          asserted.push(options);
          return Promise.resolve(aCredential(OTHER_PRF));
        },
      },
    });

    const released = await gate.release('vault');

    expect(released.outcome).toBe('refused');
  });

  it('refuses a browser whose creation reports no PRF, and stores nothing', async () => {
    Object.defineProperty(globalThis.navigator, 'credentials', {
      configurable: true,
      value: {
        create: () => Promise.resolve(aCredential(null)),
        get: () => Promise.resolve(aCredential(PRF)),
      },
    });

    await expect(gate.store('vault', 'the-purpose-key')).rejects.toThrow('PRF');

    expect(gate.enrolled('vault')).toBe(false);
  });

  it('answers no enrolment before one is stored', async () => {
    expect(await gate.release('vault')).toEqual({ outcome: 'not_enrolled' });
  });

  it('draws a machine with no platform authenticator as unavailable', async () => {
    vi.stubGlobal('PublicKeyCredential', {
      isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(false),
    });

    const availability = await gate.availability();

    expect(availability.available).toBe(false);
    expect(availability.reason).toContain('platform authenticator');
  });

  it('drops the record on remove, leaving the purpose idempotent', async () => {
    await gate.store('vault', 'the-purpose-key');

    gate.remove('vault');

    expect(gate.enrolled('vault')).toBe(false);
    expect(await gate.release('vault')).toEqual({ outcome: 'not_enrolled' });
  });
});

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

/** A create that reports PRF enabled but returns no output. */
function anEnabledCredential(): unknown {
  return {
    rawId: new Uint8Array([5, 6, 7, 8]).buffer,
    getClientExtensionResults: () => ({ prf: { enabled: true } }),
  };
}

/** A create that reports PRF disabled. */
function aDisabledCredential(): unknown {
  return {
    rawId: new Uint8Array([5, 6, 7, 8]).buffer,
    getClientExtensionResults: () => ({ prf: { enabled: false } }),
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
  // jsdom reports every page insecure; a real browser on localhost or https
  // is not, and that is the context these ceremonies run in.
  vi.stubGlobal('isSecureContext', true);
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
    // **The page's own origin is the RP.** No `rp.id` means the browser scopes
    // the credential to the effective domain of the page, which is what the
    // web client wants; the socket server's address is not involved.
    expect(create?.['rp']).toEqual({ name: 'forge' });
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

  /// **An insecure page has no WebAuthn at all**, and saying so as "this
  /// browser has no WebAuthn" sends the reader looking at the wrong thing.
  it('draws an insecure origin as its own refusal', async () => {
    vi.stubGlobal('isSecureContext', false);
    vi.stubGlobal('PublicKeyCredential', undefined);

    const availability = await gate.availability();

    expect(availability.available).toBe(false);
    expect(availability.reason).toContain('secure origin');
    expect(availability.reason).toContain('password');
  });

  /// The browser itself can say it has no PRF, before anything is created:
  /// the press is not offered and the password is named.
  it('draws a browser whose capabilities report no PRF as unavailable', async () => {
    vi.stubGlobal('PublicKeyCredential', {
      isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(true),
      getClientCapabilities: () => Promise.resolve({ 'extension:prf': false }),
    });

    const availability = await gate.availability();

    expect(availability.available).toBe(false);
    expect(availability.reason).toContain('PRF');
    expect(availability.reason).toContain('password');
  });

  it('is available when the capabilities report the PRF extension', async () => {
    vi.stubGlobal('PublicKeyCredential', {
      isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(true),
      getClientCapabilities: () => Promise.resolve({ 'extension:prf': true }),
    });

    expect(await gate.availability()).toEqual({ available: true, biometric: true });
  });

  /// **An absent key is not a false one.** A browser with no capability API
  /// (or one that omits the extension) gets the press, and the attempt
  /// settles whether the authenticator answers PRF.
  it('offers the press when the browser cannot say whether it has PRF', async () => {
    vi.stubGlobal('PublicKeyCredential', {
      isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(true),
    });

    expect(await gate.availability()).toEqual({ available: true, biometric: true });
  });

  it('draws a browser with no WebCrypto as unavailable', async () => {
    vi.stubGlobal('PublicKeyCredential', {
      isUserVerifyingPlatformAuthenticatorAvailable: () => Promise.resolve(true),
    });
    vi.stubGlobal('crypto', {});

    const availability = await gate.availability();

    expect(availability.available).toBe(false);
    expect(availability.reason).toContain('WebCrypto');
  });

  it('drops the record on remove, leaving the purpose idempotent', async () => {
    await gate.store('vault', 'the-purpose-key');

    gate.remove('vault');

    expect(gate.enrolled('vault')).toBe(false);
    expect(await gate.release('vault')).toEqual({ outcome: 'not_enrolled' });
  });

  /// **A re-enrol replaces the key, not the passkey.** The press is the route
  /// back from a stale blob, and each press must not leave another passkey in
  /// the person's own passkey manager.
  it('reuses the passkey it already made, replacing its sealed key', async () => {
    await gate.store('vault', 'first-key');
    await gate.store('vault', 'second-key');

    expect(created, 'one passkey, not two').toHaveLength(1);
    expect(asserted.length, 'the second press asserted the existing passkey').toBe(1);
    expect(await gate.release('vault')).toEqual({ outcome: 'released', blob: 'second-key' });
  });

  /// A record the passkey cannot open is dead; a fresh passkey replaces it
  /// rather than reusing a credential whose wrapping key is lost.
  it('replaces a record the passkey cannot open', async () => {
    await gate.store('vault', 'first-key');
    const held = JSON.parse(
      globalThis.localStorage.getItem('forge.biometric.web.vault') ?? '{}',
    ) as Record<string, unknown>;
    globalThis.localStorage.setItem(
      'forge.biometric.web.vault',
      JSON.stringify({ ...held, wrapped: 'AAAA' }),
    );

    await gate.store('vault', 'second-key');

    expect(created, 'the dead record was replaced by a fresh passkey').toHaveLength(2);
    expect(await gate.release('vault')).toEqual({ outcome: 'released', blob: 'second-key' });
  });

  /// **Some authenticators report PRF enabled and withhold the output at
  /// creation.** One assertion fetches it; without that step a working
  /// authenticator would read as PRF-less.
  it('asks for the PRF once when creation withholds the output', async () => {
    Object.defineProperty(globalThis.navigator, 'credentials', {
      configurable: true,
      value: {
        create: (options: Record<string, unknown>) => {
          created.push(options);
          return Promise.resolve(anEnabledCredential());
        },
        get: (options: Record<string, unknown>) => {
          asserted.push(options);
          return Promise.resolve(aCredential(PRF));
        },
      },
    });

    await gate.store('vault', 'the-purpose-key');

    expect(created).toHaveLength(1);
    expect(asserted, 'one assertion fetched the output').toHaveLength(1);
    expect(await gate.release('vault')).toEqual({ outcome: 'released', blob: 'the-purpose-key' });
  });

  /// A disabled PRF is answered, not retried: there is nothing a second
  /// assertion could return.
  it('does not assert when creation reports the PRF disabled', async () => {
    Object.defineProperty(globalThis.navigator, 'credentials', {
      configurable: true,
      value: {
        create: () => Promise.resolve(aDisabledCredential()),
        get: (options: Record<string, unknown>) => {
          asserted.push(options);
          return Promise.resolve(aCredential(PRF));
        },
      },
    });

    await expect(gate.store('vault', 'the-purpose-key')).rejects.toThrow('PRF');

    expect(asserted).toHaveLength(0);
    expect(gate.enrolled('vault')).toBe(false);
  });

  /// A record the asserted PRF cannot open is dropped, so the next unlock
  /// goes straight to the password instead of raising a gesture that cannot
  /// succeed.
  it('drops a record the asserted PRF cannot open', async () => {
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
    expect(gate.enrolled('vault'), 'the dead record goes').toBe(false);
  });
});

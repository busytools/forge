// @vitest-environment jsdom
/**
 * The vault's enrolment lifecycle: the key goes to the gate, the sealed bytes
 * to the core, and neither side alone can open a start.
 */
import { webcrypto } from 'node:crypto';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import { release, remove, store } from './index';
import { openVaultMaterial, type VaultMaterial } from './vault';
import {
  deviceName,
  enrolVault,
  forgetVaultMaterial,
  releaseVaultMaterial,
  watchVaultEnrolment,
} from './vault-enrolment';

vi.mock('./index', () => ({
  // The real one is the module's own words; a refusal in these tests is a
  // string, which is what it returns for one.
  reasonOf: (why: unknown) =>
    typeof why === 'string' ? why : 'the platform authenticator did not answer',
  release: vi.fn(),
  store: vi.fn(),
  remove: vi.fn(),
}));

const gateRelease = vi.mocked(release);
const gateStore = vi.mocked(store);
const gateRemove = vi.mocked(remove);

/**
 * jsdom has no WebCrypto subtle; the platform's own is what a webview has, so
 * the test installs Node's rather than standing in for it.
 */
beforeEach(() => {
  vi.stubGlobal('crypto', webcrypto);
  gateRelease.mockReset();
  gateStore.mockReset();
  gateRemove.mockReset();
  gateStore.mockResolvedValue(undefined);
  globalThis.localStorage.clear();
  forgetVaultMaterial();
});

const MATERIAL: VaultMaterial = { refresh_token: 'refresh-1', user_key: 'user-key-1' };

interface Recorded {
  calls: unknown[];
  listeners: ((message: ServerMessage) => void)[];
}

/** A connection double: every command answered from `answers`. */
function aConnection(answers: (command: unknown) => unknown): Recorded & {
  dispatch: (command: unknown) => Promise<unknown>;
  onMessage: (fn: (message: ServerMessage) => void) => () => void;
} {
  const recorded: Recorded = { calls: [], listeners: [] };
  return {
    ...recorded,
    dispatch: (command: unknown) => {
      recorded.calls.push(command);
      return Promise.resolve(answers(command));
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      recorded.listeners.push(fn);
      return () => {};
    },
  };
}

/** Let WebCrypto's promises and the rewrite's own awaits settle. */
async function settled(): Promise<void> {
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
}

/**
 * Poll until `what` holds. **A wait that runs out THROWS**: the rewrite's
 * WebCrypto work resolves on the event loop, so a fixed number of turns is a
 * race under load, and the deadline only stops a broken page hanging.
 */
async function until(what: () => boolean, why: string): Promise<void> {
  const deadline = Date.now() + 4_000;
  for (;;) {
    if (what()) return;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${why}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

function commandName(command: unknown): string | null {
  if (typeof command === 'string') return command;
  if (command === null || typeof command !== 'object') return null;
  const [name] = Object.keys(command);
  return name ?? null;
}

describe('enrolling this device', () => {
  it('seals the material under a key the gate holds and registers the bytes', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );

    await enrolVault(connection);

    expect(gateStore, 'the gate holds the wrapping key').toHaveBeenCalledOnce();
    const [purpose, key] = gateStore.mock.calls[0] ?? [];
    expect(purpose).toBe('vault');
    const enrol = connection.calls.find((command) => commandName(command) === 'biometric_enrol') as
      | { biometric_enrol: { enrolment: string; device: string; purpose: string; blob: string } }
      | undefined;
    expect(enrol, 'the sealed bytes are registered').toBeDefined();
    expect(enrol?.biometric_enrol.purpose).toBe('vault');
    expect(enrol?.biometric_enrol.enrolment).toMatch(/^[0-9a-f-]{36}$/);
    expect(enrol?.biometric_enrol.device).not.toBe('');
    expect(
      await openVaultMaterial(enrol?.biometric_enrol.blob ?? '', key as string),
      'the bytes open under the key the gate holds',
    ).toEqual(MATERIAL);
  });

  /// **The serial is what tells two devices of one platform apart.** The page
  /// has no hostname on any client, so the id this client minted is the one
  /// distinctive part of the name the list draws.
  it('names the device by its platform and its own serial', () => {
    expect(deviceName('abcdef12-3456-7890-abcd-ef1234567890')).toMatch(/ \(abcdef12\)$/);
  });

  it('enrols nothing while the session is locked', async () => {
    const connection = aConnection(() => null);

    await enrolVault(connection);

    expect(gateStore).not.toHaveBeenCalled();
    expect(connection.calls.some((command) => commandName(command) === 'biometric_enrol')).toBe(
      false,
    );
  });
});

describe('releasing the material', () => {
  it('joins the gate key to the core bytes', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    const [, key] = gateStore.mock.calls[0] ?? [];
    const enrolled = connection.calls.find(
      (command) => commandName(command) === 'biometric_enrol',
    ) as { biometric_enrol: { enrolment: string; blob: string } };
    gateRelease.mockResolvedValue({ outcome: 'released', blob: key as string });
    const releaseConnection = aConnection((command) => {
      if (commandName(command) === 'biometric_release') {
        return { blob: enrolled.biometric_enrol.blob };
      }
      return true;
    });

    const answer = await releaseVaultMaterial(releaseConnection);

    expect(answer).toEqual({ outcome: 'released', material: MATERIAL });
  });

  it('refuses when the stored bytes do not open under the released key', async () => {
    gateRelease.mockResolvedValue({
      outcome: 'released',
      blob: 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
    });
    const connection = aConnection((command) => {
      if (commandName(command) === 'biometric_enrol') return true;
      if (commandName(command) === 'biometric_release') return { blob: 'c2VhbGVk' };
      return MATERIAL;
    });

    const answer = await releaseVaultMaterial(connection);

    expect(answer.outcome).toBe('refused');
  });

  it('passes a refused gesture through without asking the core', async () => {
    gateRelease.mockResolvedValue({ outcome: 'refused', reason: 'the gesture was cancelled' });
    const connection = aConnection(() => null);

    const answer = await releaseVaultMaterial(connection);

    expect(answer).toEqual({ outcome: 'refused', reason: 'the gesture was cancelled' });
    expect(connection.calls).toHaveLength(0);
  });
});

describe('a rotation', () => {
  it('rewrites the sealed bytes with the new token', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    // Hold the material the way a release does.
    const [, key] = gateStore.mock.calls[0] ?? [];
    const enrolled = connection.calls.find(
      (command) => commandName(command) === 'biometric_enrol',
    ) as { biometric_enrol: { blob: string } };
    gateRelease.mockResolvedValue({ outcome: 'released', blob: key as string });
    const live = aConnection((command) => {
      if (commandName(command) === 'biometric_release') {
        return { blob: enrolled.biometric_enrol.blob };
      }
      return true;
    });
    await releaseVaultMaterial(live);
    const stop = watchVaultEnrolment(live);
    const before = live.calls.length;

    live.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await until(
      () =>
        live.calls.slice(before).some((command) => commandName(command) === 'biometric_rewrite'),
      'the blob to be rewritten',
    );

    // **The rewrite, not the enrol**: the rotation may only update a row the
    // person created, so it can never re-register a revoked device.
    const rewritten = live.calls
      .slice(before)
      .find((command) => commandName(command) === 'biometric_rewrite') as
      { biometric_rewrite: { blob: string } } | undefined;
    expect(rewritten, 'the blob is rewritten').toBeDefined();
    expect(await openVaultMaterial(rewritten?.biometric_rewrite.blob ?? '', key as string)).toEqual(
      {
        refresh_token: 'refresh-2',
        user_key: 'user-key-1',
      },
    );
    stop();
  });

  it('locks the vault when the rewrite cannot be stored', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    const [, key] = gateStore.mock.calls[0] ?? [];
    const enrolled = connection.calls.find(
      (command) => commandName(command) === 'biometric_enrol',
    ) as { biometric_enrol: { blob: string } };
    gateRelease.mockResolvedValue({ outcome: 'released', blob: key as string });
    const live = aConnection((command) => {
      if (commandName(command) === 'biometric_release') {
        return { blob: enrolled.biometric_enrol.blob };
      }
      if (commandName(command) === 'biometric_rewrite') return false;
      return true;
    });
    await releaseVaultMaterial(live);
    watchVaultEnrolment(live);
    const before = live.calls.length;

    live.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await until(() => live.calls.slice(before).includes('vault_lock'), 'the vault to lock');

    expect(live.calls.slice(before)).toContain('vault_lock');
  });

  it('does nothing when this run holds no material', async () => {
    const connection = aConnection(() => true);
    watchVaultEnrolment(connection);

    connection.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await settled();

    expect(connection.calls).toHaveLength(0);
  });

  /**
   * **A revoked device is dropped from the rewrite path.** The enrolments
   * frame names the set the core holds; when this device's id is gone, the
   * material goes, so a rotation that follows has nothing to rewrite.
   */
  it('drops the material when the enrolments frame no longer names this device', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    const [, key] = gateStore.mock.calls[0] ?? [];
    const enrolled = connection.calls.find(
      (command) => commandName(command) === 'biometric_enrol',
    ) as { biometric_enrol: { blob: string } };
    gateRelease.mockResolvedValue({ outcome: 'released', blob: key as string });
    const live = aConnection((command) => {
      if (commandName(command) === 'biometric_release') {
        return { blob: enrolled.biometric_enrol.blob };
      }
      return true;
    });
    await releaseVaultMaterial(live);
    watchVaultEnrolment(live);

    live.listeners[0]?.({
      kind: 'update',
      update: { biometric_enrolments_changed: { enrolments: [] } },
    });
    const before = live.calls.length;
    live.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await settled();

    expect(live.calls.slice(before), 'a rotation has nothing to rewrite').toHaveLength(0);
  });

  /// The released path's "the core holds no enrolment" answer is the same
  /// revocation: the local item goes and the material goes with it.
  it('forgets the material when the core holds no enrolment', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    gateRelease.mockResolvedValue({ outcome: 'released', blob: 'a2V5' });

    const live = aConnection(() => null);
    const answer = await releaseVaultMaterial(live);

    expect(answer.outcome).toBe('not_enrolled');
    expect(gateRemove, 'the local item goes with the revoked row').toHaveBeenCalledWith('vault');
    watchVaultEnrolment(live);
    const before = live.calls.length;
    live.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await settled();
    expect(live.calls.slice(before), 'and a rotation has nothing to rewrite').toHaveLength(0);
  });
});

describe("enrolment is the person's own action", () => {
  /**
   * **A lock frame is slot-less.** Every connected client sees another
   * client's unlock, so enrolling on it made every one of them a device and
   * let a revoked phone rewrite the row the settings page had just deleted.
   * The watcher must never ask for material.
   */
  it('never enrols from an unlock frame', async () => {
    const connection = aConnection(() => true);
    watchVaultEnrolment(connection);

    connection.listeners[0]?.({
      kind: 'update',
      update: { vault_lock_changed: { locked: false } },
    });
    await settled();

    expect(connection.calls).toHaveLength(0);
  });

  /// A lock drops this run's material, so a later rotation has nothing to
  /// rewrite with and does not pretend otherwise.
  it('drops the held material when the vault locks', async () => {
    const connection = aConnection((command) =>
      commandName(command) === 'vault_enrolment_material' ? MATERIAL : true,
    );
    await enrolVault(connection);
    const [, key] = gateStore.mock.calls[0] ?? [];
    const enrolled = connection.calls.find(
      (command) => commandName(command) === 'biometric_enrol',
    ) as { biometric_enrol: { blob: string } };
    gateRelease.mockResolvedValue({ outcome: 'released', blob: key as string });
    const live = aConnection((command) => {
      if (commandName(command) === 'biometric_release') {
        return { blob: enrolled.biometric_enrol.blob };
      }
      return true;
    });
    await releaseVaultMaterial(live);
    watchVaultEnrolment(live);

    live.listeners[0]?.({
      kind: 'update',
      update: { vault_lock_changed: { locked: true } },
    });
    const before = live.calls.length;
    live.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await settled();

    expect(live.calls.slice(before), 'a locked run holds nothing to rewrite with').toHaveLength(0);
  });
});

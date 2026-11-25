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
  enrolVault,
  enrolmentId,
  enrolmentIdIfMinted,
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

  /// **One id names a DEVICE, not a purpose.** A second purpose on one browser
  /// is the same device, so the id lives under a key of its own rather than
  /// one per purpose; every release and every row's revoke names it.
  it('keeps one id per device, under the device key', () => {
    const minted = enrolmentId();

    expect(minted).toMatch(/^[0-9a-f-]{36}$/);
    expect(enrolmentId()).toBe(minted);
    expect(enrolmentIdIfMinted()).toBe(minted);
    expect(globalThis.localStorage.getItem('forge.biometric.device')).toBe(minted);
    expect(
      globalThis.localStorage.getItem('forge.biometric.enrolment.vault'),
      'the purpose-scoped key is gone',
    ).toBeNull();
  });

  /// A read is a read: only enrolling mints, so a comparison against the
  /// core's list cannot create an id as a side effect.
  it('never mints an id from a read', () => {
    globalThis.localStorage.clear();

    expect(enrolmentIdIfMinted()).toBeNull();
    expect(globalThis.localStorage.length, 'and stores nothing at all').toBe(0);
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
    enrolmentId();
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

  /// **No id is no row, and a release never mints one.** A device that never
  /// enrolled has no row to name, so the passkey prompt is not raised and the
  /// card draws the password instead.
  it('answers not_enrolled without a gesture when no id was ever minted', async () => {
    globalThis.localStorage.clear();
    const connection = aConnection(() => null);

    const answer = await releaseVaultMaterial(connection);

    expect(answer).toEqual({ outcome: 'not_enrolled' });
    expect(
      gateRelease,
      'no passkey prompt for a device that never enrolled',
    ).not.toHaveBeenCalled();
    expect(connection.calls, 'and nothing is asked of the core').toHaveLength(0);
    expect(enrolmentIdIfMinted(), 'and the attempt mints no id').toBeNull();
  });

  /// **The row is read before the gesture.** A device revoked elsewhere has
  /// no row, so a gesture would be spent on a call that cannot land.
  it('reads the core row first and drops a revoked device without a gesture', async () => {
    enrolmentId();
    const connection = aConnection((command) =>
      commandName(command) === 'biometric_release' ? null : true,
    );

    const answer = await releaseVaultMaterial(connection);

    expect(answer).toEqual({ outcome: 'not_enrolled' });
    expect(gateRelease, 'no gesture for a row that is gone').not.toHaveBeenCalled();
    expect(gateRemove, 'the gate record goes with the row').toHaveBeenCalledWith('vault');
    expect(connection.calls.map(commandName)).toEqual(['biometric_release']);
  });

  /// A refused gesture is the platform's answer after the row said there is
  /// something to release.
  it('passes a refused gesture through once the row answers', async () => {
    enrolmentId();
    gateRelease.mockResolvedValue({ outcome: 'refused', reason: 'the gesture was cancelled' });
    const connection = aConnection(() => ({ blob: 'c2VhbGVk' }));

    const answer = await releaseVaultMaterial(connection);

    expect(answer).toEqual({ outcome: 'refused', reason: 'the gesture was cancelled' });
    expect(connection.calls.map(commandName)).toEqual(['biometric_release']);
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
   * **A refused write leaves nothing held.** A rotation after it must not
   * rewrite a row that was never stored, and must not lock the vault for a
   * failure it did not cause.
   */
  it('holds nothing when the enrol write is refused', async () => {
    const connection = aConnection((command) => {
      const name = commandName(command);
      if (name === 'vault_enrolment_material') return MATERIAL;
      if (name === 'biometric_enrol') return false;
      return true;
    });
    const answer = await enrolVault(connection);
    expect(answer.outcome).toBe('failed');
    expect(answer).toMatchObject({ reason: 'the core did not store the enrolment' });
    expect(
      gateRemove,
      'a gate record with no row is an orphan, so it goes with the failed write',
    ).toHaveBeenCalledWith('vault');

    const before = connection.calls.length;
    watchVaultEnrolment(connection);
    connection.listeners[0]?.({
      kind: 'update',
      update: { vault_rotated: { refresh_token: 'refresh-2' } },
    });
    await settled();

    expect(connection.calls.slice(before), 'no material is held to rewrite with').toHaveLength(0);
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

    const live = aConnection(() => null);
    const answer = await releaseVaultMaterial(live);

    expect(answer.outcome).toBe('not_enrolled');
    expect(gateRelease, 'and no gesture is asked for the missing row').not.toHaveBeenCalled();
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

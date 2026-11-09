// @vitest-environment jsdom
/**
 * The vault's enrolment lifecycle: the key goes to the gate, the sealed bytes
 * to the core, and neither side alone can open a start.
 */
import { webcrypto } from 'node:crypto';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import { release, store } from './index';
import { openVaultMaterial, type VaultMaterial } from './vault';
import {
  enrolVault,
  forgetVaultMaterial,
  releaseVaultMaterial,
  watchVaultEnrolment,
} from './vault-enrolment';

vi.mock('./index', () => ({ release: vi.fn(), store: vi.fn() }));

const gateRelease = vi.mocked(release);
const gateStore = vi.mocked(store);

/**
 * jsdom has no WebCrypto subtle; the platform's own is what a webview has, so
 * the test installs Node's rather than standing in for it.
 */
beforeEach(() => {
  vi.stubGlobal('crypto', webcrypto);
  gateRelease.mockReset();
  gateStore.mockReset();
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
      () => live.calls.slice(before).some((command) => commandName(command) === 'biometric_enrol'),
      'the blob to be registered again',
    );

    const rewritten = live.calls
      .slice(before)
      .find((command) => commandName(command) === 'biometric_enrol') as
      { biometric_enrol: { blob: string } } | undefined;
    expect(rewritten, 'the blob is registered again').toBeDefined();
    expect(await openVaultMaterial(rewritten?.biometric_enrol.blob ?? '', key as string)).toEqual({
      refresh_token: 'refresh-2',
      user_key: 'user-key-1',
    });
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
      if (commandName(command) === 'biometric_enrol') return false;
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
});

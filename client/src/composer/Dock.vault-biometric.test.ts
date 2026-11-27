// @vitest-environment jsdom
/**
 * The locked vault card's states, driven through the vault consumer's release
 * mock.
 *
 * **What the card draws at each step is the claim**: the gesture where the
 * device offers one, the password one click beneath it, the working state
 * while the OS prompt is up, and the platform's own words when the gesture
 * refuses - with the password beside them and the card still standing, because
 * a refused gesture must not cost the call the person was answering.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

/** The shell's door, recorded per command so a case can answer each one. */
const shell = vi.hoisted(() => ({
  inTauri: true,
  answers: new Map<string, (args?: Record<string, unknown>) => unknown>(),
  calls: [] as { command: string; args?: Record<string, unknown> | undefined }[],
}));

vi.mock('@tauri-apps/api/core', () => ({
  isTauri: (): boolean => shell.inTauri,
  invoke: (command: string, args?: Record<string, unknown>): Promise<unknown> => {
    shell.calls.push({ command, args });
    const answer = shell.answers.get(command);
    if (answer === undefined) return Promise.reject(new Error(`no answer for ${command}`));
    return Promise.resolve(answer(args));
  },
}));

/** The vault consumer's release, which the card calls as its primary action,
 *  and the record this client minted when it enrolled. */
const consumer = vi.hoisted(() => ({
  releaseVaultMaterial: vi.fn(),
  enrolmentIdIfMinted: vi.fn(),
}));

vi.mock('../biometric/vault-enrolment', () => ({
  releaseVaultMaterial: consumer.releaseVaultMaterial,
  enrolmentIdIfMinted: consumer.enrolmentIdIfMinted,
}));

import Dock from './Dock.svelte';
import type { ServerMessage } from '../protocol';
import type { Ask } from './wire';

const SLOT = { org: 'TestOrg', project: 'proj', label: 'lead' };

function aVaultAsk(needsUnlock = true): Ask {
  return {
    kind: 'vault',
    request: {
      id: 'vault-1',
      tool: 'vault__get',
      summary: "read the password for 'bank'",
      itemId: 'item-1',
      itemName: 'bank',
      field: 'password',
      maskedDiff: null,
      attachment: null,
      needsUnlock,
    },
  };
}

let app: Record<string, unknown> | null = null;
const dispatched: unknown[][] = [];
/** The frames the card listens for: the live lock state arrives this way. */
const listeners: ((message: ServerMessage) => void)[] = [];

function mountDock(ask: Ask): void {
  app = mount(Dock, {
    target: document.body,
    props: {
      ask,
      slot: SLOT,
      connection: {
        dispatch: (command: unknown, slot: unknown) => {
          dispatched.push([command, slot]);
          return Promise.resolve();
        },
        onMessage: (fn: (message: ServerMessage) => void) => {
          listeners.push(fn);
          return () => {
            const at = listeners.indexOf(fn);
            if (at !== -1) listeners.splice(at, 1);
          };
        },
      },
    },
  });
  flushSync();
}

/**
 * Let the gate's promises settle and redraw. The shell's `invoke` resolves on
 * a macrotask, so microtasks alone leave the card in its first paint.
 */
async function settled(): Promise<void> {
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
  flushSync();
}

function button(text: string): HTMLButtonElement | null {
  const found = Array.from(document.querySelectorAll('button')).find(
    (candidate) => candidate.textContent?.trim().startsWith(text) === true,
  );
  return found ?? null;
}

beforeEach(() => {
  dispatched.length = 0;
  listeners.length = 0;
  shell.calls.length = 0;
  shell.inTauri = true;
  shell.answers.clear();
  shell.answers.set('biometric_availability', () => ({ available: true, biometric: true }));
  consumer.releaseVaultMaterial.mockReset();
  consumer.enrolmentIdIfMinted.mockReset();
  consumer.enrolmentIdIfMinted.mockReturnValue('device-1');
});

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the locked vault card with an enrolment', () => {
  it('draws the gesture as the primary action, with the password beneath it', async () => {
    mountDock(aVaultAsk());
    await settled();

    expect(button('Unlock with this device'), 'the gesture is the primary action').not.toBeNull();
    expect(button('use the Vaultwarden password'), 'the password is one click away').not.toBeNull();
    expect(
      document.querySelector('input[type="password"]'),
      'the password fields stay closed until they are asked for',
    ).toBeNull();
    expect(
      document.activeElement?.textContent?.trim().startsWith('Unlock with this device'),
      'the keyboard lands on the gesture, so Enter runs it',
    ).toBe(true);
  });

  it('runs the gesture on Enter and sends the released material with the answer', async () => {
    consumer.releaseVaultMaterial.mockResolvedValue({
      outcome: 'released',
      material: { refresh_token: 'refresh-1', user_key: 'user-key-1' },
    });
    mountDock(aVaultAsk());
    await settled();

    const card = document.querySelector('.dock');
    if (!(card instanceof HTMLElement)) throw new Error('the dock did not draw');
    card.focus();
    card.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
    );
    await settled();

    expect(
      consumer.releaseVaultMaterial,
      'the consumer was asked to release',
    ).toHaveBeenCalledOnce();
    expect(dispatched).toHaveLength(1);
    expect(dispatched[0]?.[0]).toEqual({
      respond_vault_ask: {
        key: SLOT,
        id: 'vault-1',
        answer: {
          decision: 'approve',
          released: { refresh_token: 'refresh-1', user_key: 'user-key-1' },
        },
      },
    });
  });

  /**
   * **One approval, one release, and no keychain read to decide the drawing.**
   * The card asks the platform only whether it can answer a gesture at all;
   * whether this client holds an item comes from its own record (the id an
   * enrol minted), because asking the keychain is itself a gesture on macOS.
   * The press is the probe: a release with nothing stored answers
   * `not_enrolled` without a prompt, and the card falls to the password.
   */
  it('asks the gate only what it can answer without a gesture', async () => {
    consumer.releaseVaultMaterial.mockResolvedValue({
      outcome: 'released',
      material: { refresh_token: 'refresh-1', user_key: 'user-key-1' },
    });
    mountDock(aVaultAsk());
    await settled();

    expect(
      shell.calls.map((call) => call.command),
      'the draw asks for availability and nothing else',
    ).toEqual(['biometric_availability']);
    expect(consumer.enrolmentIdIfMinted, 'the record decides the gesture').toHaveBeenCalled();

    button('Unlock with this device')?.click();
    await settled();

    expect(consumer.releaseVaultMaterial, 'one gesture for the approval').toHaveBeenCalledTimes(1);
    expect(dispatched, 'one answer').toHaveLength(1);
  });

  it('draws the working state while the prompt is up', async () => {
    const pending: { resolve: ((answer: unknown) => void) | null } = { resolve: null };
    consumer.releaseVaultMaterial.mockReturnValue(
      new Promise((resolve) => {
        pending.resolve = resolve;
      }),
    );
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with this device')?.click();
    await settled();
    const working = button('waiting for this device');
    expect(working, 'the card says what it is waiting for').not.toBeNull();
    expect(working?.disabled, 'the button cannot be pressed twice').toBe(true);
    expect(dispatched, 'nothing is answered while the prompt stands').toHaveLength(0);

    pending.resolve?.({ outcome: 'refused', reason: 'the gesture was cancelled' });
    await settled();
    expect(button('waiting for this device'), 'the working state ends with the gesture').toBeNull();
  });

  /// **A release that throws still ends the working state**: a read that
  /// throws on a closed socket must not leave the button saying it is
  /// waiting for ever.
  it('leaves the working state when the release throws', async () => {
    consumer.releaseVaultMaterial.mockRejectedValue(new Error('the socket is closed'));
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with this device')?.click();
    await settled();

    expect(button('waiting for this device'), 'the working state ends').toBeNull();
    expect(document.body.textContent).toContain('the socket is closed');
    expect(dispatched, 'nothing is answered').toHaveLength(0);
  });

  /**
   * **A lock frame is live news.** The card's `needsUnlock` was stamped when
   * it parked, and the idle clock can land while it stands; the answer then
   * carries no password and the call dies with "Locked" unless the card puts
   * the password back.
   */
  it('asks for the password when a lock frame arrives', async () => {
    mountDock(aVaultAsk(false));
    await settled();
    expect(
      document.querySelector('input[type="password"]'),
      'an unlocked card draws no password field',
    ).toBeNull();

    for (const listener of listeners) {
      // The page reads this update by its own key, exactly as the socket
      // delivers it; the cast is the test standing in for the wire.
      listener({
        kind: 'update',
        update: { vault_lock_changed: { locked: true } },
      } as unknown as ServerMessage);
    }
    await settled();

    expect(button('Unlock with this device'), 'the card now asks for the unlock').not.toBeNull();
    expect(button('use the Vaultwarden password'), 'and offers the password path').not.toBeNull();
    expect(button('Approve'), 'a bare approve cannot run without unlock material').toBeNull();
  });

  it('keeps the card standing on a refusal, says why, and offers the password', async () => {
    consumer.releaseVaultMaterial.mockResolvedValue({
      outcome: 'refused',
      reason: 'the authenticator refused the gesture',
    });
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with this device')?.click();
    await settled();

    expect(document.body.textContent).toContain('the authenticator refused the gesture');
    expect(dispatched, 'a refused gesture never settles the call').toHaveLength(0);
    expect(button('Unlock with this device'), 'the card is still the card').not.toBeNull();

    button('use the Vaultwarden password')?.click();
    await settled();
    expect(
      document.querySelector('input[type="password"]'),
      'the password the refusal offered is drawn',
    ).not.toBeNull();
    expect(
      document.activeElement instanceof HTMLInputElement &&
        document.activeElement.type === 'password',
      'the keyboard lands in the field the refusal offered',
    ).toBe(true);
  });

  it('falls back to the password when the device has nothing enrolled', async () => {
    consumer.releaseVaultMaterial.mockResolvedValue({ outcome: 'not_enrolled' });
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with this device')?.click();
    await settled();

    expect(document.body.textContent).toContain('this device has nothing enrolled');
    expect(dispatched).toHaveLength(0);
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });
});

describe('the vault card with the vault already unlocked', () => {
  /**
   * **An unlocked vault costs no gesture at all.** The card is an ordinary
   * approve, so it asks the gate for nothing and its answer carries no unlock
   * material.
   */
  it('asks the gate for nothing and approves without unlock material', async () => {
    mountDock(aVaultAsk(false));
    await settled();

    expect(shell.calls, 'no probe while no unlock is needed').toEqual([]);
    button('Approve')?.click();
    await settled();

    expect(consumer.releaseVaultMaterial, 'no release').not.toHaveBeenCalled();
    expect(dispatched, 'one answer').toHaveLength(1);
    expect(dispatched[0]?.[0]).toEqual({
      respond_vault_ask: { key: SLOT, id: 'vault-1', answer: { decision: 'approve' } },
    });
  });
});

describe('the locked vault card without an enrolment', () => {
  it('draws the password card when this client minted no record', async () => {
    consumer.enrolmentIdIfMinted.mockReturnValue(null);
    mountDock(aVaultAsk());
    await settled();

    expect(button('Unlock with this device')).toBeNull();
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
    expect(button('Approve'), 'the password card keeps its approve').not.toBeNull();
    expect(
      shell.calls.map((call) => call.command),
      'the keychain is never read to answer this',
    ).toEqual(['biometric_availability']);
  });

  it('draws the password card where the platform has no gate', async () => {
    shell.answers.set('biometric_availability', () => ({
      available: false,
      biometric: false,
      reason: 'this platform has no biometric gate yet',
    }));
    mountDock(aVaultAsk());
    await settled();

    expect(button('Unlock with this device')).toBeNull();
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });
});

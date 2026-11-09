// @vitest-environment jsdom
/**
 * The locked vault card's states, driven through the shell's gate mock.
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

import Dock from './Dock.svelte';
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
      },
    },
  });
  flushSync();
}

/**
 * Let the gate's promises settle and redraw. The dynamic `invoke` import
 * resolves on a macrotask, so microtasks alone leave the card in its first
 * paint and every assertion reads a gate that has not answered yet.
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
  shell.calls.length = 0;
  shell.inTauri = true;
  shell.answers.clear();
  shell.answers.set('biometric_availability', () => ({ available: true, biometric: true }));
  shell.answers.set('biometric_enrolled', () => true);
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

    expect(button('Unlock with Touch ID'), 'the gesture is the primary action').not.toBeNull();
    expect(button('use the Vaultwarden password'), 'the password is one click away').not.toBeNull();
    expect(
      document.querySelector('input[type="password"]'),
      'the password fields stay closed until they are asked for',
    ).toBeNull();
    expect(
      document.activeElement?.textContent?.trim().startsWith('Unlock with Touch ID'),
      'the keyboard lands on the gesture, so Enter runs it',
    ).toBe(true);
  });

  it('runs the gesture on Enter and sends the released material with the answer', async () => {
    shell.answers.set('biometric_release', () => ({
      outcome: 'released',
      blob: JSON.stringify({ refresh_token: 'refresh-1', user_key: 'user-key-1' }),
    }));
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
      shell.calls.some((call) => call.command === 'biometric_release'),
      'the gate was asked for the purpose',
    ).toBe(true);
    expect(shell.calls.find((call) => call.command === 'biometric_release')?.args).toEqual({
      purpose: 'vault',
    });
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

  it('draws the working state while the OS prompt is up', async () => {
    const pending: { resolve: ((answer: unknown) => void) | null } = { resolve: null };
    shell.answers.set(
      'biometric_release',
      () =>
        new Promise((resolve) => {
          pending.resolve = resolve;
        }),
    );
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with Touch ID')?.click();
    await settled();
    const working = button('waiting for Touch ID');
    expect(working, 'the card says what it is waiting for').not.toBeNull();
    expect(working?.disabled, 'the button cannot be pressed twice').toBe(true);
    expect(dispatched, 'nothing is answered while the prompt stands').toHaveLength(0);

    pending.resolve?.({ outcome: 'refused', reason: 'the gesture was cancelled' });
    await settled();
    expect(button('waiting for Touch ID'), 'the working state ends with the gesture').toBeNull();
  });

  it('keeps the card standing on a refusal, says why, and offers the password', async () => {
    shell.answers.set('biometric_release', () => ({
      outcome: 'refused',
      reason: 'the authenticator refused the gesture',
    }));
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with Touch ID')?.click();
    await settled();

    expect(document.body.textContent).toContain('the authenticator refused the gesture');
    expect(dispatched, 'a refused gesture never settles the call').toHaveLength(0);
    expect(button('Unlock with Touch ID'), 'the card is still the card').not.toBeNull();

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

  it('falls back to the password when the released blob is not readable', async () => {
    shell.answers.set('biometric_release', () => ({ outcome: 'released', blob: 'not json' }));
    mountDock(aVaultAsk());
    await settled();

    button('Unlock with Touch ID')?.click();
    await settled();

    expect(document.body.textContent).toContain('could not be read; use the password');
    expect(dispatched).toHaveLength(0);
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });
});

describe('the locked vault card without an enrolment', () => {
  it('draws the password card when the device has no enrolment', async () => {
    shell.answers.set('biometric_enrolled', () => false);
    mountDock(aVaultAsk());
    await settled();

    expect(button('Unlock with Touch ID')).toBeNull();
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
    expect(button('Approve'), 'the password card keeps its approve').not.toBeNull();
  });

  it('draws the password card where the platform has no gate', async () => {
    shell.answers.set('biometric_availability', () => ({
      available: false,
      biometric: false,
      reason: 'this platform has no biometric gate yet',
    }));
    mountDock(aVaultAsk());
    await settled();

    expect(button('Unlock with Touch ID')).toBeNull();
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });
});

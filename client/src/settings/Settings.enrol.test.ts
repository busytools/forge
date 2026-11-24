// @vitest-environment jsdom
/**
 * The settings page's vault press: the person's own action, the unlock dialog
 * a locked vault opens, and what the card draws for each ending.
 *
 * **The press is the only enrolment path**, so this is where that path is
 * driven from the page rather than from the card's own props: a press that
 * stopped calling the core, or a dialog that unlocked without continuing the
 * enrol, would otherwise be invisible to every other suite.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { settingsFrame } from './frame';
import { homeWire } from '../dev/fixture.data';
import { settingsWire } from '../dev/settings.fixture';
import type { HomeRead } from '../home/live';
import type { Connection } from '../socket';
import type { SettingsDocument, SettingsKey } from '../wire/settings';
import Settings from './Settings.svelte';

const enrol = vi.hoisted(() => ({
  enrolVault: vi.fn(),
  releaseVaultMaterial: vi.fn(),
  unlockVault: vi.fn(),
}));
vi.mock('../biometric/vault-enrolment', () => ({
  enrolVault: enrol.enrolVault,
  releaseVaultMaterial: enrol.releaseVaultMaterial,
  unlockVault: enrol.unlockVault,
  enrolmentIdIfMinted: () => null,
  forgetVaultMaterial: () => {},
}));

vi.mock('../biometric/device', () => ({
  readDeviceGate: () => Promise.resolve({ outcome: 'ready' }),
}));

let app: Record<string, unknown> | null = null;

const connection = {
  onMessage: () => () => {},
  dispatch: () => Promise.resolve(),
  settingsEdit: () => true,
} as unknown as Connection;

const vaultKey: SettingsKey = {
  path: 'vault.url',
  leaf: 'url',
  group: ['vault'],
  pattern: 'vault.url',
  class: 'live',
  classLabel: 'live',
  value: { kind: 'string', text: 'https://vault.test' },
  edit: { kind: 'string', allowed: [], required: true, group: null },
};

/** The shared fixture's connectors area, plus the vault key whose group the
 *  card draws after. */
function documentWithVault(): SettingsDocument {
  return {
    ...settingsWire,
    areas: settingsWire.areas.map((area) =>
      area.id === 'connectors' ? { ...area, keys: [...area.keys, vaultKey] } : area,
    ),
  };
}

function read(vault: { locked: boolean; idle_minutes: number } | null): HomeRead {
  return {
    wire: { ...homeWire, vault, biometric: { outcome: 'read', value: [] } },
    refused: null,
    failed: null,
    report: null,
  };
}

function draw(vault: { locked: boolean; idle_minutes: number } | null): void {
  settingsFrame.set({ status: 'ready', document: documentWithVault() });
  app = mount(Settings, {
    target: document.body,
    props: { connection, home: read(vault), mark: null },
  });
  flushSync();
  const rail = [...document.querySelectorAll<HTMLButtonElement>('.settings-rail-item')].find(
    (item) => item.textContent?.includes('connectors') === true,
  );
  rail?.click();
  flushSync();
}

async function settled(): Promise<void> {
  for (let turn = 0; turn < 8; turn += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

function button(label: string): HTMLButtonElement | null {
  return (
    [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (element) => element.textContent?.trim() === label,
    ) ?? null
  );
}

function type(selector: string, value: string): void {
  const input = document.querySelector<HTMLInputElement>(selector);
  if (input === null) throw new Error(`no input at ${selector}`);
  input.value = value;
  input.dispatchEvent(new Event('input'));
  flushSync();
}

beforeEach(() => {
  enrol.enrolVault.mockReset();
  enrol.releaseVaultMaterial.mockReset();
  enrol.unlockVault.mockReset();
  enrol.unlockVault.mockResolvedValue({ outcome: 'unlocked' });
  enrol.releaseVaultMaterial.mockResolvedValue({ outcome: 'not_enrolled' });
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  document.body.innerHTML = '';
});

describe('the vault press', () => {
  it('enrols this client through the core when the vault is open', async () => {
    enrol.enrolVault.mockResolvedValue({ outcome: 'enrolled' });
    draw({ locked: false, idle_minutes: 15 });
    await settled();
    expect(button('enrol this client'), 'a ready gate offers the press').not.toBeNull();

    button('enrol this client')?.click();
    await settled();

    expect(enrol.enrolVault).toHaveBeenCalledTimes(1);
    expect(enrol.enrolVault.mock.calls[0]?.[0], 'the page hands over its own connection').toBe(
      connection,
    );
    // The server list holds no row for this client, so the card says so and
    // keeps the press: the replacing re-enrol is the route back.
    expect(document.body.textContent).toContain('enrolled here');
  });

  it('opens the unlock dialog while locked, then enrols in place', async () => {
    enrol.enrolVault.mockResolvedValue({ outcome: 'enrolled' });
    draw({ locked: true, idle_minutes: 15 });
    await settled();

    button('enrol this client')?.click();
    await settled();

    expect(document.body.textContent, 'the dialog opens, not a refusal').toContain(
      'unlock to enrol this client',
    );
    expect(enrol.enrolVault, 'nothing enrolls before the vault opens').not.toHaveBeenCalled();

    type('input[type="password"]', 'open sesame');
    type('input[autocomplete="one-time-code"]', '123456');
    button('unlock and enrol')?.click();
    await settled();

    expect(enrol.unlockVault).toHaveBeenCalledWith(connection, {
      password: 'open sesame',
      two_factor: '123456',
    });
    expect(enrol.enrolVault, 'the dialog continues into the enrol').toHaveBeenCalledTimes(1);
    expect(document.body.textContent, 'the dialog closed').not.toContain(
      'unlock to enrol this client',
    );
  });

  it('keeps the dialog open with the server reason on a refused unlock', async () => {
    enrol.unlockVault.mockResolvedValue({
      outcome: 'refused',
      reason: 'The username or password is incorrect.',
    });
    draw({ locked: true, idle_minutes: 15 });
    await settled();

    button('enrol this client')?.click();
    await settled();
    type('input[type="password"]', 'wrong');
    button('unlock and enrol')?.click();
    await settled();

    expect(document.querySelector('[role="alert"]')?.textContent).toContain('incorrect');
    expect(document.querySelector('input[type="password"]'), 'the fields stay').not.toBeNull();
    expect(enrol.enrolVault).not.toHaveBeenCalled();
  });

  it('draws the reason a failed enrolment gives', async () => {
    enrol.enrolVault.mockResolvedValue({
      outcome: 'failed',
      reason: 'the core did not store the enrolment',
    });
    draw({ locked: false, idle_minutes: 15 });
    await settled();

    button('enrol this client')?.click();
    await settled();

    expect(document.body.textContent).toContain('the core did not store the enrolment');
  });

  it('draws no door when no vault is configured', async () => {
    draw(null);
    await settled();

    expect(document.body.textContent, 'the card says the vault is absent').toContain(
      'No [vault] is configured',
    );
    expect(button('enrol this client'), 'and offers no door that can never succeed').toBeNull();
  });

  /// **One press at a time.** A second press interleaved with the first can
  /// store a gate key and then lose the write, leaving a key that opens
  /// nothing; the running state takes the door away until the first answers.
  it('draws the press as running and offers no second one', async () => {
    let finish: (answer: unknown) => void = () => {};
    enrol.enrolVault.mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    draw({ locked: false, idle_minutes: 15 });
    await settled();

    button('enrol this client')?.click();
    await settled();

    expect(document.body.textContent).toContain('enrolling this client');
    expect(button('enrol this client'), 'no second press while the first runs').toBeNull();

    finish({ outcome: 'enrolled' });
    await settled();
    expect(document.body.textContent).toContain('enrolled here');
  });

  it('draws an unread home as unread, not as a vault that is absent', () => {
    settingsFrame.set({ status: 'ready', document: documentWithVault() });
    app = mount(Settings, {
      target: document.body,
      props: {
        connection,
        home: { wire: null, refused: null, failed: null, report: null },
        mark: null,
      },
    });
    flushSync();
    const rail = [...document.querySelectorAll<HTMLButtonElement>('.settings-rail-item')].find(
      (item) => item.textContent?.includes('connectors') === true,
    );
    rail?.click();
    flushSync();

    expect(document.body.textContent).toContain('The vault state has not been read yet');
    expect(button('enrol this client'), 'no card and no door before the home').toBeNull();
  });
});

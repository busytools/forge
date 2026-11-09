// @vitest-environment jsdom
/**
 * The settings page's enrol press: the person's own action, its vault-open
 * guard, and what the device card draws for each ending.
 *
 * **The press is the only enrolment path**, so this is where that path is
 * driven from the page rather than from the card's own props: a press that
 * stopped calling the core, or a guard that let a locked vault through, would
 * otherwise be invisible to every other suite.
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

const enrol = vi.hoisted(() => ({ enrolVault: vi.fn() }));
vi.mock('../biometric/vault-enrolment', () => ({
  enrolVault: enrol.enrolVault,
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
 *  cards draw after. */
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
  for (let turn = 0; turn < 6; turn += 1) await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

function enrolButton(): HTMLButtonElement | null {
  return (
    [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (button) => button.textContent?.trim() === 'enrol',
    ) ?? null
  );
}

beforeEach(() => {
  enrol.enrolVault.mockReset();
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  settingsFrame.set({ status: 'waiting' });
  document.body.innerHTML = '';
});

describe('the enrol press', () => {
  it('enrols this device through the core and draws the ending', async () => {
    enrol.enrolVault.mockResolvedValue({ outcome: 'enrolled' });
    draw({ locked: false, idle_minutes: 15 });
    await settled();
    expect(enrolButton(), 'a ready gate offers the press').not.toBeNull();

    enrolButton()?.click();
    await settled();

    expect(enrol.enrolVault).toHaveBeenCalledTimes(1);
    expect(enrol.enrolVault.mock.calls[0]?.[0], 'the page hands over its own connection').toBe(
      connection,
    );
    // The server list holds no row for this device, so the card says so and
    // keeps the press: the replacing re-enrol is the route back.
    expect(document.body.textContent).toContain('enrolled here');
  });

  it('refuses to enrol while the vault is locked, and says why', async () => {
    draw({ locked: true, idle_minutes: 15 });
    await settled();

    enrolButton()?.click();
    await settled();

    expect(enrol.enrolVault, 'a locked session answers no material').not.toHaveBeenCalled();
    expect(document.body.textContent).toContain('not enrolled - unlock the vault first');
  });

  it('draws the reason a failed enrolment gives', async () => {
    enrol.enrolVault.mockResolvedValue({
      outcome: 'failed',
      reason: 'the core did not store the enrolment',
    });
    draw({ locked: false, idle_minutes: 15 });
    await settled();

    enrolButton()?.click();
    await settled();

    expect(document.body.textContent).toContain('the core did not store the enrolment');
  });

  it('draws no enrol door when no vault is configured', async () => {
    draw(null);
    await settled();

    expect(document.body.textContent, 'the card says the vault is absent').toContain(
      'No [vault] is configured',
    );
    expect(enrolButton(), 'and offers no door that can never succeed').toBeNull();
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
    expect(enrolButton(), 'no card and no door before the home').toBeNull();
  });
});

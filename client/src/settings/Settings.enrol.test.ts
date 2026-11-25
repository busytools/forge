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
import type { BiometricEnrolment } from '../wire/home';
import type { SettingsDocument, SettingsKey } from '../wire/settings';
import Settings from './Settings.svelte';

const enrol = vi.hoisted(() => ({
  enrolVault: vi.fn(),
  releaseVaultMaterial: vi.fn(),
  unlockVault: vi.fn(),
  remove: vi.fn(() => Promise.resolve()),
  forgetVaultMaterial: vi.fn(),
  mine: { id: null as string | null },
}));
vi.mock('../biometric', () => ({
  reasonOf: (why: unknown) => (typeof why === 'string' ? why : 'the platform did not answer'),
  remove: enrol.remove,
}));
vi.mock('../biometric/vault-enrolment', () => ({
  enrolVault: enrol.enrolVault,
  releaseVaultMaterial: enrol.releaseVaultMaterial,
  unlockVault: enrol.unlockVault,
  enrolmentIdIfMinted: () => enrol.mine.id,
  forgetVaultMaterial: enrol.forgetVaultMaterial,
}));

const gate = vi.hoisted(() => ({ readDeviceGate: vi.fn() }));
vi.mock('../biometric/device', () => ({ readDeviceGate: gate.readDeviceGate }));

const LOCAL_ID = '11111111-2222-4333-8444-555555555555';

let app: Record<string, unknown> | null = null;

const dispatch = vi.fn(() => Promise.resolve());
const connection = {
  onMessage: () => () => {},
  dispatch,
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

/** A home whose device list is the one given. */
function homeWith(
  vault: { locked: boolean; idle_minutes: number } | null,
  devices: BiometricEnrolment[],
): HomeRead {
  return {
    wire: { ...homeWire, vault, biometric: { outcome: 'read', value: devices } },
    refused: null,
    failed: null,
    report: null,
  };
}

/** One device row as the home carries it. */
function row(id: string, device: string): BiometricEnrolment {
  const at = { secs_since_epoch: 1_790_000_000, nanos_since_epoch: 0 };
  return {
    id,
    device,
    created_at: at,
    purposes: [{ purpose: 'vault', created_at: at, last_used_at: null }],
  };
}

function drawHome(home: HomeRead): void {
  settingsFrame.set({ status: 'ready', document: documentWithVault() });
  app = mount(Settings, {
    target: document.body,
    props: { connection, home, mark: null },
  });
  flushSync();
  const rail = [...document.querySelectorAll<HTMLButtonElement>('.settings-rail-item')].find(
    (item) => item.textContent?.includes('connectors') === true,
  );
  rail?.click();
  flushSync();
}

function draw(vault: { locked: boolean; idle_minutes: number } | null): void {
  drawHome(read(vault));
}

/** The same home with this client's own enrolment in the core's list. */
function readEnrolled(vault: { locked: boolean; idle_minutes: number }): HomeRead {
  const at = { secs_since_epoch: 1_790_000_000, nanos_since_epoch: 0 };
  return {
    wire: {
      ...homeWire,
      vault,
      biometric: {
        outcome: 'read',
        value: [
          {
            id: LOCAL_ID,
            device: 'this mac',
            created_at: at,
            purposes: [{ purpose: 'vault', created_at: at, last_used_at: null }],
          },
        ],
      },
    },
    refused: null,
    failed: null,
    report: null,
  };
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
  enrol.remove.mockReset();
  enrol.remove.mockResolvedValue(undefined);
  enrol.forgetVaultMaterial.mockReset();
  enrol.mine.id = null;
  dispatch.mockReset();
  dispatch.mockResolvedValue(undefined);
  gate.readDeviceGate.mockReset();
  gate.readDeviceGate.mockResolvedValue({ outcome: 'ready', biometric: true });
  localStorage.clear();
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

  it('opens straight into the gesture for an enrolled, capable client', async () => {
    gate.readDeviceGate.mockResolvedValue({ outcome: 'enrolled', biometric: true });
    enrol.mine.id = LOCAL_ID;
    enrol.releaseVaultMaterial.mockResolvedValue({
      outcome: 'released',
      material: { refresh_token: 'refresh', user_key: 'key' },
    });
    drawHome(readEnrolled({ locked: true, idle_minutes: 15 }));
    await settled();

    // **No choice screen**: the client knows this device can release the
    // vault, so the press asks for the gesture at once (Ved, 2026-11-25).
    button('unlock')?.click();
    await settled();

    expect(enrol.releaseVaultMaterial, 'the gesture was not asked at once').toHaveBeenCalledTimes(
      1,
    );
    expect(
      document.querySelector('input[type="password"]'),
      'the password was offered before the gesture failed',
    ).toBeNull();
    expect(enrol.unlockVault).toHaveBeenCalledWith(connection, {
      released: { refresh_token: 'refresh', user_key: 'key' },
    });
    expect(document.querySelector('.settings-dialog'), 'the dialog closed').toBeNull();
  });

  it('reveals the password under the reason when the gesture is refused', async () => {
    gate.readDeviceGate.mockResolvedValue({ outcome: 'enrolled', biometric: true });
    localStorage.setItem('forge.biometric.enrolment.vault', LOCAL_ID);
    enrol.releaseVaultMaterial.mockResolvedValue({
      outcome: 'refused',
      reason: 'the gesture was cancelled',
    });
    drawHome(readEnrolled({ locked: true, idle_minutes: 15 }));
    await settled();

    button('unlock')?.click();
    await settled();

    expect(document.body.textContent).toContain('the gesture was cancelled');
    expect(
      document.querySelector('input[type="password"]'),
      'the password is the way forward and was not shown',
    ).not.toBeNull();
    expect(
      document.querySelector('.settings-dialog'),
      'the dialog closed on a refusal',
    ).not.toBeNull();
  });

  it('closes the dialog on Back and returns focus to the press', async () => {
    draw({ locked: true, idle_minutes: 15 });
    await settled();
    const opener = button('enrol this client');
    // jsdom's click does not move focus, so the opener takes it first: the
    // dialog returns the keyboard to whatever held it when it opened.
    opener?.focus();
    opener?.click();
    await settled();
    expect(document.querySelector('.settings-dialog')).not.toBeNull();

    // **jsdom does not deliver `popstate` for a same-document back**, so the
    // platform's event is dispatched: it is the one the page's own handler
    // answers on a real Back.
    history.back();
    dispatchEvent(new PopStateEvent('popstate'));
    await settled();

    expect(document.querySelector('.settings-dialog'), 'Back closed the dialog').toBeNull();
    expect(document.activeElement, 'and the keyboard came back').toBe(opener);
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

  /// **This client's own record goes with its server row.** A revoked
  /// registration must not leave a gesture that can never succeed, or material
  /// a later rotation would rewrite the deleted row with.
  it("drops this client's record when its own vault row is revoked", async () => {
    enrol.mine.id = 'device-1';
    drawHome(
      homeWith({ locked: true, idle_minutes: 15 }, [
        row('device-1', 'Chromium on macOS (device1)'),
        row('device-2', 'phone'),
      ]),
    );
    await settled();

    button('revoke')?.click();
    await vi.waitFor(() => expect(dispatch).toHaveBeenCalled());

    expect(dispatch).toHaveBeenCalledWith({
      biometric_revoke: { enrolment: 'device-1', purpose: 'vault' },
    });
    expect(enrol.forgetVaultMaterial, 'the held material goes').toHaveBeenCalled();
    expect(enrol.remove, "and the gate's record goes").toHaveBeenCalledWith('vault');
  });

  it("leaves this client's record alone when another device is revoked", async () => {
    enrol.mine.id = 'device-1';
    drawHome(homeWith({ locked: true, idle_minutes: 15 }, [row('device-2', 'phone')]));
    await settled();

    button('revoke')?.click();
    // **Wait for the handler to have run** before asserting it did nothing:
    // a fixed settle can pass on a slow machine before the code could act.
    await vi.waitFor(() => expect(dispatch).toHaveBeenCalled());

    expect(dispatch).toHaveBeenCalledWith({
      biometric_revoke: { enrolment: 'device-2', purpose: 'vault' },
    });
    expect(enrol.forgetVaultMaterial).not.toHaveBeenCalled();
    expect(enrol.remove).not.toHaveBeenCalled();
  });
});

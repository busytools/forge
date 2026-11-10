// @vitest-environment jsdom
/**
 * The vault area's device enrolments: the list the home carries, its empty
 * and failed states, and the revoke a row sends.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import BiometricCard from './BiometricCard.svelte';
import type { DeviceGate } from '../biometric/device';
import { enrolmentId } from '../biometric/vault-enrolment';
import type { BiometricEnrolment } from '../wire/home';

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

const AT = { secs_since_epoch: 1_790_000_000, nanos_since_epoch: 0 };

function devices(): BiometricEnrolment[] {
  return [
    {
      id: 'device-1',
      device: "Ved's MacBook Pro",
      created_at: AT,
      purposes: [
        {
          purpose: 'vault',
          created_at: AT,
          last_used_at: { ...AT, secs_since_epoch: 1_790_003_600 },
        },
      ],
    },
    {
      id: 'device-2',
      device: 'phone',
      created_at: AT,
      purposes: [{ purpose: 'vault', created_at: AT, last_used_at: null }],
    },
  ];
}

function draw(
  read: { outcome: 'read'; value: BiometricEnrolment[] } | { outcome: 'failed'; value: string },
  onrevoke: (enrolment: string, purpose: string) => void = () => {},
  device?: DeviceGate,
  onenrol: () => void = () => {},
): void {
  app = mount(BiometricCard, {
    target: document.body,
    props: { read, onrevoke, ...(device === undefined ? {} : { device }), onenrol },
  });
  flushSync();
}

function enrolButton(): HTMLButtonElement | null {
  return (
    [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (button) => button.textContent?.trim() === 'enrol',
    ) ?? null
  );
}

function revokeButtons(): HTMLButtonElement[] {
  return [...document.querySelectorAll<HTMLButtonElement>('button')].filter(
    (button) => button.textContent?.trim() === 'revoke',
  );
}

describe('the device enrolments card', () => {
  it('lists every device and purpose, with the release stamps', () => {
    draw({ outcome: 'read', value: devices() });

    expect(document.body.textContent).toContain("Ved's MacBook Pro");
    expect(document.body.textContent).toContain('phone');
    expect(document.body.textContent).toContain('vault');
    expect(document.body.textContent).toContain('last used');
    expect(document.body.textContent, 'a never-used purpose says so').toContain('never used');
    expect(revokeButtons(), 'each purpose has its own revoke').toHaveLength(2);
  });

  it('sends the enrolment and purpose of the row whose revoke was pressed', () => {
    const revoked = vi.fn();
    draw({ outcome: 'read', value: devices() }, revoked);

    revokeButtons()[1]?.click();

    expect(revoked).toHaveBeenCalledExactlyOnceWith('device-2', 'vault');
  });

  it('draws an empty list as no device enrolled', () => {
    draw({ outcome: 'read', value: [] });

    expect(document.body.textContent).toContain('No device has enrolled');
    expect(revokeButtons()).toHaveLength(0);
  });

  it('draws an unreadable list as its own state, not as none', () => {
    draw({ outcome: 'failed', value: 'the machine-local store is not open' });

    expect(document.body.textContent).toContain('could not be read');
    expect(document.body.textContent).toContain('the machine-local store is not open');
    expect(document.body.textContent).not.toContain('No device has enrolled');
  });

  it('claims no count when the list could not be read', () => {
    draw({ outcome: 'failed', value: 'the machine-local store is not open' });

    expect(
      document.querySelector('.settings-card-count'),
      'a failed read is not zero devices',
    ).toBeNull();
  });

  it('draws an unavailable gate and offers no press that cannot land', () => {
    draw({ outcome: 'read', value: [] }, () => {}, {
      outcome: 'unavailable',
      reason: 'the app is not signed for the keychain',
    });

    expect(document.body.textContent).toContain(
      'cannot unlock here: the app is not signed for the keychain',
    );
    expect(enrolButton(), 'an unavailable gate offers no door').toBeNull();
  });

  /// **The replacing re-enrol is the route back from a lost row**: the local
  /// item survives and the server row does not, so the card must keep the
  /// press even though the gate says it is enrolled.
  it('offers a replacing enrol when the server holds no row for this device', () => {
    draw({ outcome: 'read', value: [] }, () => {}, { outcome: 'enrolled' });

    expect(document.body.textContent).toContain('enrolled here, but the server has no row');
    expect(enrolButton(), 'the press is the route back').not.toBeNull();
  });

  /// **The press is a replacing re-enrol even when both sides agree**, which
  /// is the route back from a stale blob: the local item cannot tell a stale
  /// blob from a current one, so hiding the press would hide that route too.
  it('keeps the press when the local item and the server row agree', () => {
    const mine = enrolmentId();
    if (mine === null) throw new Error('the test has no storage');
    draw(
      {
        outcome: 'read',
        value: [
          {
            id: mine,
            device: 'this device',
            created_at: AT,
            purposes: [{ purpose: 'vault', created_at: AT, last_used_at: null }],
          },
        ],
      },
      () => {},
      { outcome: 'enrolled' },
    );

    expect(document.body.textContent).toContain('enrolled - the gesture unlocks the vault here');
    expect(enrolButton(), 'a replacing re-enrol is still offered').not.toBeNull();
  });

  /// **A failed read is not "the server has no row".** The claim waits for a
  /// list that was read; the press is still offered, because it replaces.
  it('makes no row claim when the list could not be read', () => {
    draw({ outcome: 'failed', value: 'the store is closed' }, () => {}, { outcome: 'enrolled' });

    expect(document.body.textContent).not.toContain('the server has no row');
    expect(document.body.textContent).toContain('enrolled - the gesture unlocks the vault here');
    expect(enrolButton(), 'the replacing press is still the route back').not.toBeNull();
  });

  it('draws the press as running, with no door to press again', () => {
    draw({ outcome: 'read', value: [] }, () => {}, { outcome: 'enrolling' });

    expect(document.body.textContent).toContain('enrolling this device');
    expect(enrolButton(), 'one press at a time').toBeNull();
  });
});

// @vitest-environment jsdom
/**
 * The vault area's device enrolments: the list the home carries, its empty
 * and failed states, and the revoke a row sends.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import BiometricCard from './BiometricCard.svelte';
import type { DeviceGate } from '../biometric/device';
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

  /// **The replacing re-enrol is the route back from a missed rotation**: the
  /// local item survives and the server row does not, so the card must keep
  /// the press even though the gate says it is enrolled.
  it('offers a replenishing enrol when the server holds no row for this device', () => {
    draw({ outcome: 'read', value: [] }, () => {}, { outcome: 'enrolled' });

    expect(document.body.textContent).toContain('enrolled here, but the server has no row');
    expect(enrolButton(), 'the press is the route back').not.toBeNull();
  });
});

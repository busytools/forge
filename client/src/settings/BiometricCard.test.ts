// @vitest-environment jsdom
/**
 * The vault area's device enrolments: the list the home carries, its empty
 * and failed states, and the revoke a row sends.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import BiometricCard from './BiometricCard.svelte';
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
): void {
  app = mount(BiometricCard, { target: document.body, props: { read, onrevoke } });
  flushSync();
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
});

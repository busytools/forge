// @vitest-environment jsdom
/**
 * The merged vault card: the session line, the clients block, and the one
 * action this client's state offers. The state table is the spec's; each row
 * is drawn here so a missing or wrong action is a failing test rather than a
 * dead end on screen.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import VaultCard from './VaultCard.svelte';
import type { DeviceGate } from '../biometric/device';
import type { BiometricEnrolment } from '../wire/home';

const AT = { secs_since_epoch: 1_790_000_000, nanos_since_epoch: 0 };
const LOCAL_ID = '11111111-2222-4333-8444-555555555555';
/** The key the client mints its enrolment id under, as the module keeps it. */
const ENROLMENT_KEY = 'forge.biometric.enrolment.vault';

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app) {
    void unmount(app);
    app = null;
  }
  document.body.innerHTML = '';
  localStorage.clear();
});

function enrolment(id: string, device: string): BiometricEnrolment {
  return {
    id,
    device,
    created_at: AT,
    purposes: [{ purpose: 'vault', created_at: AT, last_used_at: null }],
  };
}

function draw(options: {
  device: DeviceGate;
  locked: boolean;
  clients?: BiometricEnrolment[];
  read?: { outcome: 'failed'; value: string };
}): void {
  const read = options.read ?? { outcome: 'read' as const, value: options.clients ?? [] };
  app = mount(VaultCard, {
    target: document.body,
    props: {
      vault: { locked: options.locked, idle_minutes: 15 },
      read,
      device: options.device,
      onaction: () => {},
      onrevoke: () => {},
      onlock: () => {},
    },
  });
  flushSync();
}

function button(label: string): HTMLButtonElement | null {
  return (
    [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (element) => element.textContent?.trim() === label,
    ) ?? null
  );
}

describe('the merged vault card', () => {
  it('offers the enrol press when the client is not enrolled, locked or not', () => {
    draw({ device: { outcome: 'ready', biometric: true }, locked: false });
    expect(document.body.textContent).toContain('not enrolled');
    expect(button('enrol this client')).not.toBeNull();

    document.body.innerHTML = '';
    draw({ device: { outcome: 'ready', biometric: true }, locked: true });
    expect(document.body.textContent).toContain('not enrolled');
    expect(button('enrol this client')).not.toBeNull();
  });

  it('offers unlock only while an enrolled client faces a locked vault', () => {
    localStorage.setItem(ENROLMENT_KEY, LOCAL_ID);
    draw({
      device: { outcome: 'enrolled', biometric: true },
      locked: true,
      clients: [enrolment(LOCAL_ID, 'this mac')],
    });
    expect(document.body.textContent).toContain('enrolled - the gesture unlocks the vault here');
    expect(button('unlock')).not.toBeNull();

    document.body.innerHTML = '';
    localStorage.setItem(ENROLMENT_KEY, LOCAL_ID);
    draw({
      device: { outcome: 'enrolled', biometric: true },
      locked: false,
      clients: [enrolment(LOCAL_ID, 'this mac')],
    });
    expect(document.body.textContent).toContain('enrolled - the gesture unlocks the vault here');
    expect(button('unlock'), 'an open vault has no unlock press').toBeNull();
    expect(button('lock'), 'the session line keeps its lock door').not.toBeNull();
  });

  it('offers enrol again when the server has no row for an enrolled client', () => {
    draw({ device: { outcome: 'enrolled', biometric: true }, locked: true, clients: [] });

    expect(document.body.textContent).toContain(
      'enrolled here, but the server has no row - enrol again',
    );
    expect(button('enrol again')).not.toBeNull();
  });

  it('draws the platform reason and no press when the gate cannot answer', () => {
    draw({
      device: {
        outcome: 'unavailable',
        reason: 'the app is not signed for the keychain it stores the blob in',
      },
      locked: true,
    });

    expect(document.body.textContent).toContain(
      'cannot unlock here: the app is not signed for the keychain it stores the blob in',
    );
    expect(button('enrol this client')).toBeNull();
    expect(button('enrol again')).toBeNull();
    expect(button('unlock')).toBeNull();
  });

  it('draws the enrolling and failed states with their own press', () => {
    draw({ device: { outcome: 'enrolling' }, locked: false });
    expect(document.body.textContent).toContain('enrolling this client');
    expect(button('enrol this client')).toBeNull();

    document.body.innerHTML = '';
    draw({
      device: { outcome: 'failed', reason: 'the keychain refused the item' },
      locked: false,
    });
    expect(document.body.textContent).toContain(
      'the enrolment did not land: the keychain refused the item',
    );
    expect(button('enrol again')).not.toBeNull();
  });

  it('marks this client in the list and revokes by its enrolment id', () => {
    localStorage.setItem(ENROLMENT_KEY, LOCAL_ID);
    const onrevoke = vi.fn();
    app = mount(VaultCard, {
      target: document.body,
      props: {
        vault: { locked: false, idle_minutes: 15 },
        read: {
          outcome: 'read' as const,
          value: [enrolment(LOCAL_ID, 'this mac'), enrolment('other-id', 'the other mac')],
        },
        device: { outcome: 'enrolled' as const, biometric: true },
        onaction: () => {},
        onrevoke,
        onlock: () => {},
      },
    });
    flushSync();

    expect(document.body.textContent).toContain('this client');
    expect(document.body.textContent).toContain('the other mac');
    // **The clients block wears the page's own head vocabulary**, not a bare
    // class: the head band, its name and the count.
    expect(document.querySelector('.settings-card-name')?.textContent).toBe('clients');
    expect(document.querySelector('.settings-card-count')?.textContent).toBe('2');

    const revokes = [...document.querySelectorAll<HTMLButtonElement>('button')].filter(
      (element) => element.textContent?.trim() === 'revoke',
    );
    revokes[0]?.click();
    expect(onrevoke).toHaveBeenCalledWith(LOCAL_ID, 'vault');
  });

  it('draws the empty list as its own padded line', () => {
    draw({ device: { outcome: 'ready', biometric: true }, locked: false });

    const empty = document.querySelector('.settings-card .settings-empty');
    expect(empty?.textContent).toContain('No client has enrolled');
  });

  it('draws absent and unread vaults differently', () => {
    app = mount(VaultCard, {
      target: document.body,
      props: {
        vault: null,
        read: { outcome: 'read' as const, value: [] },
        device: { outcome: 'ready' as const, biometric: true },
      },
    });
    flushSync();
    expect(document.body.textContent).toContain('No [vault] is configured');
    expect(button('enrol this client')).toBeNull();

    void unmount(app);
    document.body.innerHTML = '';
    app = mount(VaultCard, {
      target: document.body,
      props: {
        vault: undefined,
        read: { outcome: 'read' as const, value: [] },
        device: { outcome: 'ready' as const, biometric: true },
      },
    });
    flushSync();
    expect(document.body.textContent).toContain('The vault state has not been read yet');
  });
});

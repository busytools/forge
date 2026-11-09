// @vitest-environment jsdom
/**
 * The vault area's account card: the lock state, the idle bound, and the
 * manual lock.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import VaultLockCard from './VaultLockCard.svelte';
import type { VaultWire } from '../wire/home';

let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

function draw(state: VaultWire | null, onlock: () => void = () => {}): void {
  app = mount(VaultLockCard, { target: document.body, props: { state, onlock } });
  flushSync();
}

function lockButton(): HTMLButtonElement | null {
  return (
    [...document.querySelectorAll<HTMLButtonElement>('button')].find(
      (button) => button.textContent?.trim() === 'lock',
    ) ?? null
  );
}

describe('the vault card', () => {
  it('draws an unlocked session with its idle bound and the manual lock', () => {
    const locked = vi.fn();
    draw({ locked: false, idle_minutes: 30 }, locked);

    expect(document.body.textContent).toContain('unlocked');
    expect(document.body.textContent).toContain('locks after 30 minutes without use');
    const button = lockButton();
    expect(button, 'an unlocked session can be locked by hand').not.toBeNull();
    button?.click();
    expect(locked).toHaveBeenCalledOnce();
  });

  it('draws a locked session without a lock button', () => {
    draw({ locked: true, idle_minutes: null });

    expect(document.body.textContent).toContain('locked');
    expect(document.body.textContent).toContain('no idle lock');
    expect(document.body.textContent).toContain('asks for the master password');
    expect(lockButton()).toBeNull();
  });

  it('draws no-vault as its own state', () => {
    draw(null);

    expect(document.body.textContent).toContain('No [vault] is configured');
    expect(lockButton()).toBeNull();
  });
});

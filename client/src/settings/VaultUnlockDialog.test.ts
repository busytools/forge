// @vitest-environment jsdom
/**
 * The settings page's vault unlock dialog: the gesture-first variant, the
 * password path beneath it, the enrol variant that opens on the password, and
 * the refusal that never closes the dialog.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import VaultUnlockDialog from './VaultUnlockDialog.svelte';

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app) {
    void unmount(app);
    app = null;
  }
  document.body.innerHTML = '';
});

function draw(props: {
  variant: 'unlock' | 'enrol';
  busy?: boolean;
  reason?: string | null;
  ongesture?: () => void;
  onpassword?: (password: string, code: string) => void;
  oncancel: () => void;
}): void {
  app = mount(VaultUnlockDialog, { target: document.body, props });
  flushSync();
}

function button(label: string): HTMLButtonElement {
  const found = [...document.querySelectorAll<HTMLButtonElement>('button')].find((element) =>
    (element.textContent ?? '').includes(label),
  );
  if (found === undefined) throw new Error(`no button reading ${label}`);
  return found;
}

describe('the vault unlock dialog', () => {
  it('leads with the gesture and hides the password fields until the link', () => {
    draw({
      variant: 'unlock',
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });

    expect(button('Unlock with this device')).toBeDefined();
    expect(document.querySelector('input[type="password"]')).toBeNull();

    button('use the Vaultwarden password').click();
    flushSync();

    expect(document.querySelector('input[type="password"]')).not.toBeNull();
    expect(document.querySelector('input[autocomplete="one-time-code"]')).not.toBeNull();
  });

  it('draws the platform reason and keeps the password path', () => {
    draw({
      variant: 'unlock',
      reason: 'the gesture was cancelled',
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });
    button('use the Vaultwarden password').click();
    flushSync();

    expect(document.querySelector('[role="alert"]')?.textContent).toContain(
      'the gesture was cancelled',
    );
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });

  it('opens the enrol variant on the password and sends both fields', () => {
    const onpassword = vi.fn();
    draw({ variant: 'enrol', ongesture: vi.fn(), onpassword, oncancel: vi.fn() });

    expect(button('unlock and enrol')).toBeDefined();

    const password = document.querySelector<HTMLInputElement>('input[type="password"]');
    const code = document.querySelector<HTMLInputElement>('input[autocomplete="one-time-code"]');
    if (password === null || code === null) throw new Error('the fields are drawn');
    password.value = 'open sesame';
    password.dispatchEvent(new Event('input'));
    code.value = '123456';
    code.dispatchEvent(new Event('input'));
    flushSync();

    button('unlock and enrol').click();
    expect(onpassword).toHaveBeenCalledWith('open sesame', '123456');
  });

  it('draws a refusal inside the dialog while it stays open', () => {
    draw({
      variant: 'enrol',
      reason: 'The username or password is incorrect.',
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });

    expect(document.querySelector('[role="alert"]')?.textContent).toContain('incorrect');
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
  });

  it('closes on Escape through the page that owns the history entry', () => {
    const oncancel = vi.fn();
    draw({ variant: 'unlock', ongesture: vi.fn(), onpassword: vi.fn(), oncancel });

    dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));

    expect(oncancel).toHaveBeenCalledTimes(1);
  });
});

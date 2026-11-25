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
  biometric?: boolean;
  gesture?: boolean;
  fallback?: string | null;
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

  it('names the device password when the gate has no biometric', () => {
    draw({
      variant: 'unlock',
      biometric: false,
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });

    expect(button('Unlock with the device password')).toBeDefined();
  });

  /**
   * **The third shape: a gate that cannot answer.** No gesture button is
   * drawn at all, the body says why, and the dialog opens on the password
   * rather than offering a press that can only fail.
   */
  it('drops the gesture and opens on the password when the gate cannot answer', async () => {
    draw({
      variant: 'unlock',
      gesture: false,
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });
    await Promise.resolve();

    expect(document.body.textContent).toContain(
      'This device has no gesture to offer, so enter the Vaultwarden password.',
    );
    expect(
      [...document.querySelectorAll('button')].some((element) =>
        (element.textContent ?? '').includes('Unlock with'),
      ),
      'no gesture button is drawn',
    ).toBe(false);
    expect(document.querySelector('input[type="password"]')).not.toBeNull();
    expect(document.activeElement?.getAttribute('type')).toBe('password');
  });

  /**
   * **The error line is the actions' own sibling, not their spacer.** The
   * gap before the actions is the actions' margin (pinned in `sheet.test.ts`);
   * this pins the order that lets it stay constant: with a reason, the line
   * sits immediately before the actions; without one, the fields do.
   */
  it('keeps the error line immediately before the actions in both variants', () => {
    for (const variant of ['unlock', 'enrol'] as const) {
      draw({
        variant,
        reason: 'The username or password is incorrect.',
        ongesture: vi.fn(),
        onpassword: vi.fn(),
        oncancel: vi.fn(),
      });
      const alert = document.querySelector('[role="alert"]');
      const acts = document.querySelector('.acts.vault-acts');
      expect(alert, `${variant} draws the reason`).not.toBeNull();
      expect(alert?.nextElementSibling, `${variant}: the actions follow the reason`).toBe(acts);
      if (app) {
        void unmount(app);
        app = null;
      }
      document.body.innerHTML = '';
    }
  });

  it('keeps the fields immediately before the actions when no reason is drawn', () => {
    draw({ variant: 'enrol', ongesture: vi.fn(), onpassword: vi.fn(), oncancel: vi.fn() });

    const acts = document.querySelector('.acts.vault-acts');
    expect(document.querySelector('[role="alert"]'), 'no reason is drawn').toBeNull();
    expect(
      acts?.previousElementSibling?.classList.contains('fields'),
      'the fields touch the actions through their own gap',
    ).toBe(true);
  });

  it('keeps Tab inside the dialog', () => {
    draw({ variant: 'enrol', ongesture: vi.fn(), onpassword: vi.fn(), oncancel: vi.fn() });
    const focusable = [
      ...document.querySelectorAll<HTMLElement>('.settings-dialog button, .settings-dialog input'),
    ].filter((element) => !element.hasAttribute('disabled'));
    const first = focusable[0];
    const last = focusable[focusable.length - 1];

    last?.focus();
    dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab' }));

    expect(document.activeElement).toBe(first);
  });

  /** Enter submits the password path, the way the dock's unlock card does;
   *  a busy dialog does not send a second one. */
  it('submits on Enter, once, and not while busy', async () => {
    const onpassword = vi.fn();
    draw({ variant: 'enrol', ongesture: vi.fn(), onpassword, oncancel: vi.fn() });
    await Promise.resolve();
    document
      .querySelector<HTMLInputElement>('input[type="password"]')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    expect(onpassword).toHaveBeenCalledTimes(1);

    document.body.innerHTML = '';
    if (app) {
      void unmount(app);
      app = null;
    }
    const busy = vi.fn();
    draw({ variant: 'enrol', busy: true, ongesture: vi.fn(), onpassword: busy, oncancel: vi.fn() });
    await Promise.resolve();
    document
      .querySelector<HTMLInputElement>('input[type="password"]')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    expect(busy, 'a busy dialog does not submit again').not.toHaveBeenCalled();
  });

  it('starts the enrol variant in the password box', async () => {
    draw({ variant: 'enrol', ongesture: vi.fn(), onpassword: vi.fn(), oncancel: vi.fn() });
    await Promise.resolve();

    expect(document.activeElement?.getAttribute('type')).toBe('password');
  });

  /** **A close with the opener gone lands on the card head**, which is why
   *  its heading takes `tabindex="-1"`: `focus()` on a plain heading is a
   *  no-op and the keyboard fell back to `body`. */
  it('returns focus to the card head when the opener is gone', () => {
    const head = document.createElement('h3');
    head.id = 'vault-lock';
    head.tabIndex = -1;
    head.textContent = 'vault';
    document.body.append(head);
    const opener = document.createElement('button');
    document.body.append(opener);
    opener.focus();

    draw({
      variant: 'unlock',
      fallback: 'vault-lock',
      ongesture: vi.fn(),
      onpassword: vi.fn(),
      oncancel: vi.fn(),
    });
    opener.remove();
    if (app) {
      void unmount(app);
      app = null;
    }

    expect(document.activeElement).toBe(head);
  });

  it('closes on Escape through the page that owns the history entry', () => {
    const oncancel = vi.fn();
    draw({ variant: 'unlock', ongesture: vi.fn(), onpassword: vi.fn(), oncancel });

    dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));

    expect(oncancel).toHaveBeenCalledTimes(1);
  });
});

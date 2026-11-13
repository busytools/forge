// @vitest-environment jsdom
/**
 * The page-drawn picker: the trigger, the sheet, and the three doors out of it
 * (a choice, Escape, and the hardware Back that pops its history entry).
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import Picker from './Picker.svelte';
import type { PickOption } from './picker';

const SEATS: PickOption[] = [
  { value: 'a', label: 'forge \u{b7} lead', note: 'lead' },
  { value: 'b', label: 'busymail \u{b7} growth', note: 'worker' },
  { value: 'c', label: 'companies \u{b7} lead', note: 'lead' },
];

let app: Record<string, unknown> | null = null;

/** A bounded wait: a history pop is an asynchronous browser event. */
async function until(ok: () => boolean, what: string): Promise<void> {
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline) {
    flushSync();
    if (ok()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${what}`);
}

function mountPicker(overrides: Record<string, unknown> = {}): { chosen: string[] } {
  const chosen: string[] = [];
  app = mount(Picker, {
    target: document.body,
    props: {
      label: 'to',
      options: SEATS,
      value: 'b',
      onchange: (next: string) => chosen.push(next),
      title: 'choose a seat',
      ...overrides,
    },
  });
  flushSync();
  return { chosen };
}

function trigger(): HTMLButtonElement {
  const button = document.querySelector<HTMLButtonElement>('.pick');
  if (button === null) throw new Error('the picker drew no trigger');
  return button;
}

function sheet(): HTMLElement | null {
  return document.querySelector<HTMLElement>('.sheet');
}

function press(element: HTMLElement | null): void {
  if (element === null) throw new Error('nothing to press');
  element.click();
  flushSync();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the picker', () => {
  it('draws the current choice on its trigger', () => {
    mountPicker();
    expect(trigger().textContent).toContain('busymail \u{b7} growth');
    expect(trigger().getAttribute('aria-label'), 'the label is the accessible name').toBe('to');
    expect(trigger().getAttribute('aria-haspopup')).toBe('dialog');
    expect(sheet(), 'the sheet starts closed').toBeNull();
  });

  it('opens on the current row, marked with the tick and aria-current', () => {
    mountPicker();
    press(trigger());

    const dialog = sheet();
    expect(dialog, 'the sheet opened').not.toBeNull();
    expect(dialog?.getAttribute('aria-modal')).toBe('true');
    expect(dialog?.textContent).toContain('choose a seat');

    const marked = dialog?.querySelector('.opt[aria-current="true"]');
    expect(marked?.textContent, 'the current seat is the marked row').toContain(
      'busymail \u{b7} growth',
    );
    expect(marked?.innerHTML, 'and it wears the house tick').toContain('i-check');
    expect(document.activeElement, 'the keyboard starts on the answer given').toBe(marked);
  });

  it('answers the choice and hands the keyboard back to the trigger', async () => {
    const { chosen } = mountPicker();
    press(trigger());
    const rows = [...document.querySelectorAll<HTMLElement>('.opt')];
    press(rows[2] ?? null);

    expect(chosen).toEqual(['c']);
    await until(() => sheet() === null, 'the sheet to close');
    expect(sheet(), 'the choice closed the sheet').toBeNull();
    expect(document.activeElement, 'the keyboard returned to the trigger').toBe(trigger());
  });

  it("keeps the hardware Back's own door: a pop closes it", () => {
    mountPicker();
    press(trigger());
    expect(sheet()).not.toBeNull();

    // Android's Back is a pop: the entry this picker took is consumed.
    window.dispatchEvent(new PopStateEvent('popstate'));
    flushSync();
    expect(sheet(), 'the pop closed the sheet').toBeNull();
    expect(document.activeElement).toBe(trigger());
  });

  /**
   * **The veil is this control's own**, not the session rail's `.scrim` - a
   * class whose global rule keeps it invisible and off the pointer until the
   * rail opens. A tap on the veil closes the sheet.
   */
  it('closes when the veil is tapped', async () => {
    mountPicker();
    press(trigger());
    const veil = document.querySelector<HTMLElement>('.veil');
    expect(veil, 'the sheet draws a veil over the page').not.toBeNull();

    veil?.click();
    await until(() => sheet() === null, 'the veil to close the sheet');
    expect(sheet(), 'the veil closed the sheet').toBeNull();
  });

  it('closes on Escape', async () => {
    mountPicker();
    press(trigger());
    const dialog = sheet();
    if (dialog === null) throw new Error('the sheet did not open');

    dialog.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await until(() => sheet() === null, 'Escape to close the sheet');
    expect(sheet(), 'Escape closed the sheet').toBeNull();
  });

  it('does not open while it is disabled', () => {
    mountPicker({ disabled: true });
    expect(trigger().disabled).toBe(true);
    press(trigger());
    expect(sheet(), 'a frozen picker draws no sheet').toBeNull();
  });
});

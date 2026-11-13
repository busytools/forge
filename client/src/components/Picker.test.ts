// @vitest-environment jsdom
/**
 * The page-drawn picker: the trigger, the sheet, and the three doors out of it
 * (a choice, Escape, and the hardware Back that pops its history entry).
 */

import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { violationsOf } from '../testing/axe';
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
  // A page landmark, so the axe audit can judge the control as the page
  // mounts it rather than flagging bare content on the body.
  const page = document.createElement('main');
  document.body.append(page);
  app = mount(Picker, {
    target: page,
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

/** Whether the current history entry carries any picker's mark. */
function marked(): boolean {
  const held: unknown = history.state;
  const state = typeof held === 'object' && held !== null ? held : {};
  return Object.keys(state).some((key) => key.startsWith('forgePicker'));
}

/** One Tab press, as a browser delivers it. */
function pressTab(element: Element, shift = false): void {
  element.dispatchEvent(
    new KeyboardEvent('keydown', {
      key: 'Tab',
      shiftKey: shift,
      bubbles: true,
      cancelable: true,
    }),
  );
  flushSync();
}

beforeEach(() => {
  // **A clean entry per case.** jsdom keeps one history across a file, and a
  // case that leaves the sheet open pops its entry asynchronously on unmount;
  // without this the next case can inherit a mark and read it as its own.
  history.replaceState(null, '', location.href);
});

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

describe('the picker', () => {
  it('draws the current choice on its trigger, and names it too', () => {
    mountPicker();
    expect(trigger().textContent).toContain('busymail \u{b7} growth');
    expect(
      trigger().getAttribute('aria-label'),
      'the name carries the field and the seat, not the field alone',
    ).toBe('to: busymail \u{b7} growth');
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

    const marked = dialog?.querySelector('.pickopt[aria-current="true"]');
    expect(marked?.textContent, 'the current seat is the marked row').toContain(
      'busymail \u{b7} growth',
    );
    expect(marked?.innerHTML, 'and it wears the house tick').toContain('i-check');
    expect(document.activeElement, 'the keyboard starts on the answer given').toBe(marked);
  });

  it('answers the choice and hands the keyboard back to the trigger', async () => {
    const { chosen } = mountPicker();
    press(trigger());
    const rows = [...document.querySelectorAll<HTMLElement>('.pickopt')];
    press(rows[2] ?? null);

    expect(chosen).toEqual(['c']);
    await until(() => sheet() === null, 'the sheet to close');
    expect(sheet(), 'the choice closed the sheet').toBeNull();
    expect(document.activeElement, 'the keyboard returned to the trigger').toBe(trigger());
    expect(marked(), 'the choice consumed its history entry').toBe(false);
  });

  /**
   * **The hardware Back is the entry, not a synthetic event.** Opening must
   * push one, and Back must consume it; a test that dispatches popstate
   * itself would pass with the push or the back branch deleted.
   */
  it("keeps the hardware Back's own door: a real pop closes it", async () => {
    mountPicker();
    press(trigger());
    expect(sheet()).not.toBeNull();
    // The entry's own state is the check jsdom can answer: this picker marked
    // the history entry it pushed.
    expect(marked(), "opening took a history entry carrying this picker's mark").toBe(true);

    history.back();
    await until(() => sheet() === null, 'the hardware Back to close the sheet');
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
    expect(marked(), 'the veil consumed the entry').toBe(false);
  });

  /** The cancel row is the sheet's own door, beside Escape and the veil. */
  it('closes from the cancel row, and hands the keyboard back', async () => {
    mountPicker();
    press(trigger());

    const cancel = document.querySelector<HTMLButtonElement>('.cancel');
    expect(cancel, 'the sheet draws a cancel row').not.toBeNull();
    press(cancel);

    await until(() => sheet() === null, 'cancel to close the sheet');
    expect(sheet(), 'cancel closed the sheet').toBeNull();
    expect(document.activeElement, 'and the keyboard went back to the trigger').toBe(trigger());
    // **The entry is consumed, not stranded.** A close that only set the
    // state would leave the entry on top and swallow the reader's next Back.
    const held: unknown = history.state;
    const state = typeof held === 'object' && held !== null ? held : {};
    expect(
      Object.keys(state).some((key) => key.startsWith('forgePicker')),
      'the cancelled sheet popped its own history entry',
    ).toBe(false);
  });

  /**
   * **A long roster scrolls inside the sheet.** The grab bar, the title and
   * cancel stay pinned; only the rows move, so the way out cannot be pushed
   * off a phone screen by a fleet's worth of seats.
   */
  it('bounds a long list to a scrolling rows region between pinned ends', () => {
    const many: PickOption[] = Array.from({ length: 20 }, (_, at) => ({
      value: `s${String(at)}`,
      label: `seat ${String(at)}`,
    }));
    mountPicker({ options: many, value: 's0' });
    press(trigger());

    const opts = document.querySelector<HTMLElement>('.opts');
    expect(opts, 'the rows live in their own region').not.toBeNull();
    expect(opts?.querySelectorAll('.pickopt').length, 'every seat is offered').toBe(20);
    expect(opts?.contains(document.querySelector('.sheet h2')), 'the title is pinned outside').toBe(
      false,
    );
    expect(opts?.contains(document.querySelector('.cancel')), 'cancel is pinned outside').toBe(
      false,
    );
    expect(
      document.activeElement?.textContent,
      'the keyboard still lands on the current row',
    ).toContain('seat 0');
  });

  /**
   * **Escape is the window's, not the sheet's.** A click on the grab bar or
   * the title leaves focus on the container in Chrome and on the body in
   * WebKit; a listener on the sheet hears neither, so Escape must come from
   * the window.
   */
  it('closes on Escape even with the keyboard outside the sheet', async () => {
    mountPicker();
    press(trigger());
    expect(sheet(), 'the sheet did not open').not.toBeNull();

    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await until(() => sheet() === null, 'Escape to close the sheet');
    expect(sheet(), 'Escape closed the sheet from outside it').toBeNull();
    expect(marked(), 'and consumed the entry').toBe(false);
  });

  /**
   * **Tab cannot walk out of the dialog.** The sheet's own container counts
   * as its first stop for Shift+Tab and its last for Tab, so a reader whose
   * focus landed on the container (a click on the grab bar or title) stays
   * inside rather than stepping onto the trigger behind the veil.
   */
  it('wraps Tab at both ends, including from the sheet itself', () => {
    mountPicker();
    press(trigger());
    const dialog = sheet();
    if (dialog === null) throw new Error('the sheet did not open');
    const rows = [...dialog.querySelectorAll<HTMLElement>('.pickopt')];
    const cancel = dialog.querySelector<HTMLElement>('.cancel');
    const first = rows[0];
    if (first === undefined || cancel === null) throw new Error('the sheet lost its stops');

    cancel.focus();
    pressTab(cancel);
    expect(document.activeElement, 'Tab from the last stop wraps to the first').toBe(first);

    first.focus();
    pressTab(first, true);
    expect(document.activeElement, 'Shift+Tab from the first wraps to the last').toBe(cancel);

    dialog.focus();
    pressTab(dialog);
    expect(document.activeElement, 'Tab from the container goes to the first stop').toBe(first);

    dialog.focus();
    pressTab(dialog, true);
    expect(document.activeElement, 'Shift+Tab from the container goes to the last').toBe(cancel);
  });

  /**
   * **Focus outside the dialog is out of bounds.** A WebKit click on the grab
   * bar or the title leaves the body holding focus; the next Tab must come
   * back into the sheet rather than walking the page behind the veil.
   */
  it('pulls the keyboard back in when focus is outside the sheet', () => {
    mountPicker();
    press(trigger());
    const dialog = sheet();
    if (dialog === null) throw new Error('the sheet did not open');
    const first = dialog.querySelector<HTMLElement>('.pickopt');
    const cancel = dialog.querySelector<HTMLElement>('.cancel');
    if (first === undefined || cancel === null) throw new Error('the sheet lost its stops');

    (document.activeElement as HTMLElement | null)?.blur();
    expect(document.activeElement, 'focus is outside the sheet').toBe(document.body);
    pressTab(document.body);
    expect(document.activeElement, 'Tab comes back to the first stop').toBe(first);

    (document.activeElement as HTMLElement | null)?.blur();
    pressTab(document.body, true);
    expect(document.activeElement, 'Shift+Tab comes back to the last stop').toBe(cancel);
  });

  /** An empty roster still puts the keyboard in the dialog, never behind it. */
  it('focuses the sheet itself when the roster is empty', () => {
    mountPicker({ options: [], value: null });
    press(trigger());
    const dialog = sheet();
    if (dialog === null) throw new Error('the sheet did not open');
    expect(document.activeElement, 'the empty sheet holds the keyboard').toBe(dialog);
  });

  /** **The sheet is a dialog a screen reader can audit** like every other
   *  share state: no axe violations, and its own accessible name. */
  it('draws an audit-clean dialog with its own name', async () => {
    mountPicker();
    press(trigger());
    expect(sheet()?.getAttribute('aria-label'), 'the dialog names itself').toBe('choose a seat');
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  /**
   * **A pop on the way out is not left stranded.** The first press pops the
   * entry; a second press while that pop is in flight must not issue another
   * back and traverse past the share page.
   */
  it('issues one pop when the cancel row is pressed twice', async () => {
    mountPicker();
    press(trigger());
    const cancel = document.querySelector<HTMLButtonElement>('.cancel');
    if (cancel === null) throw new Error('the sheet drew no cancel row');

    const back = vi.spyOn(history, 'back');
    try {
      cancel.click();
      cancel.click();
      flushSync();
      await until(() => sheet() === null, 'the double press to settle');
      expect(back, 'the second press is not another back').toHaveBeenCalledTimes(1);
      expect(marked(), 'and the entry is consumed once').toBe(false);
    } finally {
      back.mockRestore();
    }
  });

  /**
   * **A close already in flight owns the pop.** The entry still reads as
   * current until the queued pop lands, so teardown must not issue a second
   * back and traverse past the share page.
   */
  it('does not back twice when the page unmounts while a close settles', async () => {
    mountPicker();
    press(trigger());
    const cancel = document.querySelector<HTMLButtonElement>('.cancel');
    if (cancel === null) throw new Error('the sheet drew no cancel row');

    const back = vi.spyOn(history, 'back');
    try {
      cancel.click();
      flushSync();
      if (app === null) throw new Error('nothing mounted');
      await unmount(app);
      app = null;
      flushSync();
      expect(back, 'teardown does not back a second time').toHaveBeenCalledTimes(1);
      await until(() => !marked(), 'the queued pop to land');
    } finally {
      back.mockRestore();
    }
  });

  /**
   * **The page unmounting under an open sheet drops its entry.** A second
   * share replaces the page; the popstate listener goes with it, and a
   * stranded entry swallows the next Back.
   */
  it('pops its entry when the page unmounts under it', async () => {
    mountPicker();
    press(trigger());
    expect(marked()).toBe(true);

    if (app === null) throw new Error('nothing mounted');
    await unmount(app);
    app = null;
    await until(() => !marked(), 'the unmounted sheet to drop its entry');
    expect(marked(), 'the unmounted sheet left no entry behind').toBe(false);
  });

  /**
   * **An entry buried by a route push is dropped on the pop that lands on
   * it**, rather than sitting as a step that shows nothing.
   */
  it('drops a buried entry on the next Back', async () => {
    mountPicker();
    press(trigger());
    expect(marked()).toBe(true);
    // A notification-driven route push lands above the sheet's entry.
    history.pushState({ route: 'notification' }, '', location.href);

    if (app === null) throw new Error('nothing mounted');
    await unmount(app);
    app = null;

    // **The traversal, not the state, is the event.** The current entry has
    // no mark already, so a mark-only wait passes before the pop lands; the
    // base entry's null state is what says the drop actually ran.
    history.back();
    await until(() => history.state === null, 'the buried entry to be traversed past');
    expect(marked(), 'the buried entry did not eat a Back').toBe(false);
  });

  it('does not open while it is disabled', () => {
    mountPicker({ disabled: true });
    expect(trigger().disabled).toBe(true);
    press(trigger());
    expect(sheet(), 'a frozen picker draws no sheet').toBeNull();
  });
});

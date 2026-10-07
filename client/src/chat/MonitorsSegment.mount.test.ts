// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import type { MonitorStripRow } from '../session/view';
import MonitorsSegment from './MonitorsSegment.svelte';
import { monitors } from './monitors.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its siblings pin, because it is their sibling by construction: the
 * hover-into-the-gap crossing, the close grace, focus opening and Escape
 * closing (toggle and rows, with focus returned), the tap whose synthesised
 * enter must not eat its click, and the mouseup that lets a press go.
 *
 * On top of the machine it pins what is this row's own: the ring a running
 * monitor wears, the check a settled one does, and the command drawn under
 * each row.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  monitors.sync(null);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

const MONITORS: MonitorStripRow[] = [
  {
    id: 'm1',
    running: true,
    name: 'ci-watch',
    label: 'persistent',
    command: 'gh run watch 18234567',
  },
  {
    id: 'm2',
    running: false,
    name: 'log-tail',
    label: 'stopped 2m',
    command: 'tail -f forge.log',
  },
];

function draw(rows: MonitorStripRow[] = MONITORS) {
  monitors.sync(rows);
  app = mount(MonitorsSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the monitors row's interaction state machine", () => {
  it('reads a tap as open: the synthesised enter must not eat the click', () => {
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'touch'));
    toggle()?.click();
    flushSync();

    expect(list(), 'the first tap leaves the list open').not.toBeNull();
  });

  it('reads a tap as open when the press itself focuses the toggle first', () => {
    draw();
    window.dispatchEvent(pointer('pointerdown', 'touch'));
    window.dispatchEvent(pointer('pointerup', 'touch'));
    toggle()?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    toggle()?.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    toggle()?.click();
    flushSync();

    expect(list(), 'the tap opens and stays open').not.toBeNull();
  });

  it('opens on hover and closes after the grace when the pointer leaves', () => {
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    expect(list(), 'hover opens it').not.toBeNull();

    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    expect(list(), 'still open inside the grace').not.toBeNull();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'and closed after it').toBeNull();
  });

  it('opens on a keyboard focus after a press that focused nothing', () => {
    draw();
    toggle()?.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    toggle()?.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
    flushSync();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();

    expect(list(), 'the keyboard focus still opens it').not.toBeNull();
  });

  it('opens on focus, and Escape closes it and puts focus back on the toggle', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opens it').not.toBeNull();

    toggle()?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });

  it('keeps the list while focus moves inside it, and closes on a row Escape', () => {
    draw();
    toggle()?.click();
    flushSync();
    const row = rows()[0];
    expect(row, 'the list drew its row').not.toBeUndefined();
    if (row === undefined) return;

    row.focus();
    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: row }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'focus inside keeps it').not.toBeNull();

    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape on a row closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });

  it('keeps the list when a pointer leave lands while a row holds focus', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    const row = rows()[0];
    row?.focus();
    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();

    expect(list(), 'the focused row keeps it').not.toBeNull();
  });

  it('closes when focus leaves the segment entirely', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it').not.toBeNull();

    toggle()?.dispatchEvent(
      new FocusEvent('focusout', { bubbles: true, relatedTarget: document.body }),
    );
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'a focus that left the segment closes it').toBeNull();
  });

  it('closes a list the toggle opened when the toggle is clicked again', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it, visibly').not.toBeNull();

    toggle()?.click();
    flushSync();

    expect(list(), 'the activation closes what the reader saw open').toBeNull();
  });

  it('draws the mark per state, with the command under each row', () => {
    draw();
    toggle()?.click();
    flushSync();

    expect(rows(), 'one row per monitor').toHaveLength(2);
    expect(rows()[0]?.querySelector('.ring'), 'a running monitor wears the ring').not.toBeNull();
    expect(rows()[1]?.querySelector('.ic.ok'), 'a settled one wears the check').not.toBeNull();
    expect(rows()[1]?.classList.contains('settled')).toBe(true);
    expect(rows()[0]?.querySelector('.nm')?.textContent?.trim()).toBe('ci-watch');
    expect(rows()[0]?.querySelector('.n')?.textContent?.trim()).toBe('persistent');
    const subs = [...document.querySelectorAll<HTMLElement>('.sg-sub')];
    expect(subs.map((sub) => sub.textContent?.trim())).toEqual([
      '$ gh run watch 18234567',
      '$ tail -f forge.log',
    ]);
  });

  it('holds when the pointer enters the panel over a sub-line, not a row', () => {
    // The command lines carry no row of their own, and entering the panel
    // over one must not leave the close armed from the gap crossing.
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    const grp = document.querySelector('.sg-grp');
    expect(grp, 'the list drew its groups').not.toBeNull();
    expect(grp?.querySelector('.sg-sub'), 'a group carries its command line').not.toBeNull();

    grp?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();

    expect(list(), 'the entry holds the panel open').not.toBeNull();
  });

  it('closes on a row pick and asks for nothing', () => {
    draw();
    toggle()?.click();
    flushSync();
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the pick closes the list').toBeNull();
    expect(get(scrollAsk), 'and asks the column for nothing').toBeNull();
  });
});

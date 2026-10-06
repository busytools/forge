// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import SchedulesSegment from './SchedulesSegment.svelte';
import { schedules } from './schedules.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its three strip siblings pin, because it is their sibling by
 * construction: the hover-into-the-gap crossing, the close grace, focus
 * opening and Escape closing (toggle and rows, with focus returned), the tap
 * whose synthesised enter must not eat its click, the mouseup that lets a
 * press go, and the pointer-leave that must not close over a focused row.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  schedules.sync(null);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

function draw(
  rows = [
    { id: 'c-1', key: 'rules sweep', value: 'in 27d \u{b7} recurring' },
    { id: 'c-2', key: 'plugin audit', value: 'in 27d \u{b7} one-shot' },
  ],
) {
  schedules.sync(rows);
  app = mount(SchedulesSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the schedules row's interaction state machine", () => {
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
    // A press on the already-focused toggle: no focusin consumes the press,
    // so the mouseup is what lets go - without it the next Tab would find
    // the door shut.
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
    // Opened by FOCUS rather than a tap, because the pointer really has left
    // the segment here - a click would leave jsdom matching `:hover`, where
    // the crossing guard shadows the row guard under test.
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

  it('closes on a row pick and asks for nothing: a schedule has no destination', () => {
    draw();
    toggle()?.click();
    flushSync();
    expect(rows()[0]?.textContent, 'the row reads as its key and value').toContain('rules sweep');
    expect(
      rows()[0]?.querySelector('.nm')?.textContent?.trim(),
      'the key wears the accent class the sibling rows lead with',
    ).toBe('rules sweep');
    expect(
      rows()[0]?.querySelector('.nm')?.classList.contains('lead'),
      'and takes the row, so a long description elides instead of pushing the countdown out',
    ).toBe(true);
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the pick closes the list').toBeNull();
    expect(get(scrollAsk), 'and asks the column for nothing').toBeNull();
  });

  it('draws two schedules reading the same words without colliding', () => {
    // Two crons can share a description (or a first prompt line), and rows
    // keyed by the drawn words throw on exactly that pair.
    draw([
      { id: 'c-1', key: 'rules sweep', value: 'in 27d \u{b7} recurring' },
      { id: 'c-2', key: 'rules sweep', value: 'in 2h \u{b7} one-shot' },
    ]);
    toggle()?.click();
    flushSync();

    expect(rows(), 'both schedules drew').toHaveLength(2);
  });
});

// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import type { TaskStripRow } from '../session/view';
import TasksSegment from './TasksSegment.svelte';
import { tasks } from './tasks.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its siblings pin, because it is their sibling by construction: the
 * hover-into-the-gap crossing, the close grace, focus opening and Escape
 * closing (toggle and rows, with focus returned), the tap whose synthesised
 * enter must not eat its click, and the mouseup that lets a press go.
 *
 * On top of the machine it pins what is this row's own: a mark per state, the
 * lit row a change leaves behind, and rows keyed by the task's own id - two
 * tasks can share a subject.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  tasks.sync(null);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

const TASKS: TaskStripRow[] = [
  {
    id: 't1',
    status: 'in_progress',
    display: 'Landing the schedules row',
    subject: 'Land the schedules row',
    owner: 'lead',
    meta: 'in progress \u{b7} 2h',
  },
  {
    id: 't2',
    status: 'waiting',
    display: 'Port the cmdline rule',
    subject: 'Port the cmdline rule',
    owner: 'builder',
    meta: 'waiting \u{b7} on a design call',
  },
  {
    id: 't3',
    status: 'completed',
    display: 'Draw the connector row',
    subject: 'Draw the connector row',
    owner: null,
    meta: 'completed',
  },
  {
    id: 't4',
    status: 'pending',
    display: 'Decide the collapse rule',
    subject: 'Decide the collapse rule',
    owner: null,
    meta: 'pending',
  },
];

function draw(rows: TaskStripRow[] = TASKS) {
  tasks.sync(rows);
  app = mount(TasksSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the tasks row's interaction state machine", () => {
  it('counts the finished tasks on the toggle', () => {
    draw();
    // One of the four is completed, so a count read off the wrong side
    // reads three: the fixture is asymmetric on purpose.
    expect(toggle()?.textContent, 'the toggle states the finished count').toContain('1 of 4');
  });

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

  it('draws a mark per state, the subject in accent, and the facts under it', () => {
    draw();
    toggle()?.click();
    flushSync();

    expect(rows(), 'one row per task').toHaveLength(4);
    expect(rows()[0]?.querySelector('.ring'), 'in progress wears the live ring').not.toBeNull();
    expect(
      rows()[0]?.querySelector('.nm')?.textContent?.trim(),
      'a running row leads with its active form, in the accent the sibling rows use',
    ).toBe('Landing the schedules row');
    expect(
      rows()[0]?.querySelector('.nm')?.classList.contains('lead'),
      'and takes the row, so a long subject elides instead of pushing the owner out',
    ).toBe(true);
    expect(rows()[0]?.querySelector('.n')?.textContent?.trim()).toBe('lead');
    expect(rows()[1]?.querySelector('.wait'), 'waiting wears the bars').not.toBeNull();
    expect(rows()[2]?.querySelector('.ic.ok'), 'completed wears the check').not.toBeNull();
    expect(rows()[2]?.classList.contains('settled'), 'and reads as settled').toBe(true);
    expect(rows()[3]?.querySelector('.hollow'), 'pending wears the hollow dot').not.toBeNull();
    const subs = [...document.querySelectorAll<HTMLElement>('.sg-sub')];
    expect(subs[0]?.textContent?.trim(), 'the facts ride a line under the row').toBe(
      'in progress \u{b7} 2h',
    );
  });

  /**
   * **A task that moved lights its row for a beat.** The lit id is the task's
   * own, and the class draws the same tint the reveal leaves - so the row
   * itself is the "something happened" signal, without the list being watched.
   */
  it('lights the row of a task that moved since the last read', () => {
    draw();
    toggle()?.click();
    flushSync();
    expect(rows()[0]?.classList.contains('hit'), 'nothing moved yet').toBe(false);

    // The same task, moved. The store lights it; the row draws it.
    tasks.sync(TASKS.map((row) => (row.id === 't1' ? { ...row, status: 'completed' } : row)));
    flushSync();

    const lit = rows().find((row) => row.classList.contains('hit'));
    expect(lit, 'the moved task lit no row').not.toBeUndefined();
    expect(
      lit?.querySelector('.nm')?.textContent?.trim(),
      'and it is the row of the task that moved',
    ).toBe('Landing the schedules row');

    vi.advanceTimersByTime(6500);
    flushSync();
    expect(
      rows().some((row) => row.classList.contains('hit')),
      'the light stayed on past its beat',
    ).toBe(false);
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

  it('draws two tasks reading the same subject without colliding', () => {
    // Rows key on the task's own id, and the subject is what they share:
    // keyed by a drawn word, this pair throws.
    draw([
      {
        id: 't1',
        status: 'in_progress',
        display: 'the same words',
        subject: 'the same words',
        owner: 'a',
        meta: 'in progress',
      },
      {
        id: 't2',
        status: 'pending',
        display: 'the same words',
        subject: 'the same words',
        owner: 'b',
        meta: 'pending',
      },
    ]);
    toggle()?.click();
    flushSync();

    expect(rows(), 'both tasks drew').toHaveLength(2);
  });
});

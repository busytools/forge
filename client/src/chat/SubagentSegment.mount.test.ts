// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { SubagentCard } from '../session/wire';
import SubagentSegment from './SubagentSegment.svelte';
import { subagents } from './subagents.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold.
 *
 * Everything here is the runtime's: a hover that holds across the gap into
 * the list, the grace that closes after a leave, focus opening the list and
 * Escape closing it, and the tap path a finger takes - where the browser
 * synthesises a pointer enter before the click, and an unguarded hover arm
 * makes that click toggle the panel straight back shut.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  subagents.sync(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

/** One card the list will show: running, so reachability cannot hide it. */
const card = (over: Partial<SubagentCard> = {}): SubagentCard => ({
  name: 'review the fold',
  dispatch_id: 'toolu_task',
  agent_type: 'code-reviewer',
  running: true,
  failed: false,
  backgrounded: false,
  ended_at: null,
  calls: 2,
  tail: [],
  usage: null,
  ...over,
});

function draw(cards: SubagentCard[] = [card()]) {
  subagents.sync(cards);
  subagents.syncReachable(new Set(cards.map((one) => one.dispatch_id)));
  app = mount(SubagentSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the segment's interaction state machine", () => {
  it('reads a tap as open: the synthesised enter must not eat the click', () => {
    draw();
    // A finger: pointerenter arrives with pointerType touch, then the click.
    toggle()?.dispatchEvent(pointer('pointerenter', 'touch'));
    toggle()?.click();
    flushSync();

    expect(list(), 'the first tap leaves the list open').not.toBeNull();
  });

  it('reads a tap as open when the press itself focuses the toggle first', () => {
    // Chromium's measured tap order: pointerdown and pointerup complete
    // first, then the compat mousedown, the focus it causes, and the click.
    // The focus a press puts there must not open the list for the click to
    // shut - an open the reader never saw.
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

  it('reads Enter on a list the focus opened as the toggle it visibly is', () => {
    // Tab opened the list; the reader sees it. Enter then toggles the same
    // seen state a mouse click does, rather than acting on an unseen flip.
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it, visibly').not.toBeNull();

    toggle()?.click();
    flushSync();
    expect(list(), 'the activation closes what the reader saw open').toBeNull();
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

  it('opens on focus, and Escape closes it and puts focus back on the toggle', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opens it').not.toBeNull();

    toggle()?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape closes it').toBeNull();
  });

  it('keeps the list while focus moves inside it, and drops it when focus leaves', () => {
    draw();
    toggle()?.click();
    flushSync();
    const row = document.querySelector<HTMLButtonElement>('.sg-it');
    expect(row, 'the list drew its row').not.toBeNull();
    if (row === null) return;

    // Tabbing from the toggle into a row: a bubbling focusout whose related
    // target is inside the segment is a crossing, not a leave.
    row.focus();
    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: row }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'focus inside keeps it').not.toBeNull();
  });

  it('keeps the list when a pointer leave lands while a row holds focus', () => {
    // Blink can fire the toggle's pointerleave while focus has moved into the
    // list: closing there would unmount the row the reader is on. Opened by
    // FOCUS rather than a tap, because the pointer really has left the
    // segment in this case - a click would leave jsdom matching `:hover`,
    // where the crossing guard shadows the row guard under test.
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    const row = document.querySelector<HTMLButtonElement>('.sg-it');
    row?.focus();
    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();

    expect(list(), 'the focused row keeps it').not.toBeNull();
  });

  it('lists only the instances the page can reach', () => {
    draw([card({ dispatch_id: 'gone', running: false })]);
    // `draw` seeds every card as reachable; the page holds no dispatch here.
    subagents.syncReachable(new Set());
    flushSync();
    toggle()?.click();
    flushSync();

    // The counts are the session's, the list is not: a settled instance whose
    // row the conversation does not hold leads nowhere and is not listed.
    expect(list(), 'the panel drew').not.toBeNull();
    expect(document.querySelector('.sg-it'), 'and holds no row').toBeNull();
  });

  it('closes the panel when a row is picked, with or without a column to jump to', () => {
    // The row's own side effect is the ask; the column owns the scroll, and
    // what it does with the ask is Chat's to pin. What this holds is that the
    // pick closes the panel and asks without a row present to reveal.
    draw();
    toggle()?.click();
    flushSync();
    const row = document.querySelector<HTMLButtonElement>('.sg-it');
    row?.click();
    flushSync();

    expect(list(), 'the panel closes on a pick').toBeNull();
  });
});

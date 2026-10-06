// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import ConnectorsSegment from './ConnectorsSegment.svelte';
import { connectors } from './connectors.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its two strip siblings pin, because it is their sibling by
 * construction: the hover-into-the-gap crossing, the close grace, focus
 * opening and Escape closing, the tap whose synthesised enter must not eat
 * its click, and the rows' own focus and Escape.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  connectors.sync(null);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

function draw(
  rows = [
    { kind: 'gotify' as const, id: 'g-1', key: 'client-alerts', value: '>=5' },
    {
      kind: 'slack' as const,
      id: 's-1',
      key: 'forge',
      value: 'mentions anywhere \u{b7} mentions only',
    },
  ],
) {
  connectors.sync(rows);
  app = mount(ConnectorsSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the connectors row's interaction state machine", () => {
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

  it('keeps the list while focus moves inside it, closes on a row Escape, and on a leave', () => {
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

    // And a Tab that leaves the segment entirely closes the reopened panel.
    // The relatedTarget names where focus went - body, outside the segment -
    // because a null one falls back to the live activeElement, which the row
    // Escape above deliberately parked on the toggle.
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus reopened it').not.toBeNull();
    toggle()?.dispatchEvent(
      new FocusEvent('focusout', { bubbles: true, relatedTarget: document.body }),
    );
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'a focus that left the segment closes it').toBeNull();
  });

  it('closes on a row pick and asks for nothing: a subscription has no destination', () => {
    draw();
    toggle()?.click();
    flushSync();
    expect(
      rows()[0]?.textContent,
      'the row reads as its key and value, separators spaced where they have them',
    ).toContain('client-alerts');
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the pick closes the list').toBeNull();
    expect(get(scrollAsk), 'and asks the column for nothing').toBeNull();
  });

  it("leads each row with its connector's glyph, so slack reads from gotify", () => {
    draw();
    toggle()?.click();
    flushSync();

    expect(
      rows()[0]?.querySelector('use')?.getAttribute('href'),
      'a gotify row wears the gotify glyph',
    ).toBe('#i-gotify');
    expect(
      rows()[1]?.querySelector('use')?.getAttribute('href'),
      'a slack row wears the slack glyph',
    ).toBe('#i-slack');
  });

  it('draws two subscriptions that read the same words without colliding', () => {
    // The live crash shape: a mentions watcher beside the auto-subscribed
    // conversation in one workspace draws the same words twice, and rows
    // keyed by those words throw. Keyed by the subscription's own id, both
    // draw.
    draw([
      {
        kind: 'slack' as const,
        id: 's-1',
        key: 'forge',
        value: 'mentions anywhere \u{b7} mentions only',
      },
      {
        kind: 'slack' as const,
        id: 's-2',
        key: 'forge',
        value: 'forge \u{b7} every message',
      },
    ]);
    toggle()?.click();
    flushSync();

    expect(rows(), 'both subscriptions drew').toHaveLength(2);
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

  it('closes a list the toggle opened when the toggle is clicked again', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it, visibly').not.toBeNull();

    toggle()?.click();
    flushSync();

    expect(list(), 'the activation closes what the reader saw open').toBeNull();
  });

  it('returns focus to the toggle when a row Escape closes the list', () => {
    draw();
    toggle()?.click();
    flushSync();
    const row = rows()[0];
    row?.focus();
    row?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();

    expect(list(), 'Escape on a row closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });
});

// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import type { McpRow } from '../session/view';
import McpSegment from './McpSegment.svelte';
import { mcp } from './mcp.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards its four strip siblings pin, because it is their sibling by
 * construction: the hover-into-the-gap crossing, the close grace, focus
 * opening and Escape closing (toggle and rows, with focus returned), the tap
 * whose synthesised enter must not eat its click, the mouseup that lets a
 * press go, and the pointer-leave that must not close over a focused row.
 *
 * On top of the machine it pins what is this row's own: the status depth the
 * inspector's section used to hold - the tools, the command, the failure
 * reason - drawn in the panel.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  mcp.sync(null);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

const SERVERS: McpRow[] = [
  {
    name: 'forge',
    k: 'forge \u{b7} session',
    v: '24 tools',
    tools: [
      { name: 'roster', description: null },
      { name: 'session', description: null },
    ],
    command: 'node /opt/mcp-servers/forge-server.js',
    reason: null,
  },
  {
    name: 'context7',
    k: 'context7 \u{b7} sdk',
    v: '2 tools',
    tools: [
      { name: 'query-docs', description: 'Ask the docs' },
      { name: 'resolve-library-id', description: null },
    ],
    command: 'npx -y @upstash/context7-mcp',
    reason: null,
  },
  {
    name: 'vercel',
    k: 'vercel \u{b7} session',
    v: 'needs sign-in',
    tools: [],
    command: null,
    reason: 'OAuth token expired',
  },
];

function draw(rows: McpRow[] = SERVERS) {
  mcp.sync(rows);
  app = mount(McpSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];
const subs = () => [...document.querySelectorAll<HTMLElement>('.sg-sub')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the MCP row's interaction state machine", () => {
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

  it('draws each server with its state, and the status depth under it', () => {
    draw();
    toggle()?.click();
    flushSync();

    expect(rows(), 'one row per server').toHaveLength(3);
    expect(
      rows()[0]?.querySelector('.nm')?.textContent?.trim(),
      'the name and scope lead, in the accent the sibling rows use',
    ).toBe('forge \u{b7} session');
    expect(
      rows()[0]?.querySelector('.nm')?.classList.contains('lead'),
      'and take the row, so a long pair elides instead of pushing the state out',
    ).toBe(true);
    expect(
      rows()[0]?.querySelector('.n')?.textContent?.trim(),
      'the state rides the right-hand figure',
    ).toBe('24 tools');
    expect(
      subs().map((sub) => sub.textContent?.trim()),
      'the panel carries what backs each server and what it offers',
    ).toEqual([
      'runs node /opt/mcp-servers/forge-server.js',
      'tools roster, session',
      'runs npx -y @upstash/context7-mcp',
      'tools query-docs, resolve-library-id',
      'OAuth token expired',
    ]);
    expect(subs()[4]?.classList.contains('bad'), 'a failure reason draws in the bad tone').toBe(
      true,
    );
  });

  it('holds when the pointer enters the panel over a sub-line, not a row', () => {
    // The sub-lines carry no row of their own, and entering the panel over
    // one used to leave the close armed from the gap crossing: the panel
    // shut before a hand could arrive.
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    toggle()?.dispatchEvent(pointer('pointerleave', 'mouse'));
    flushSync();
    const grp = document.querySelector('.sg-grp');
    expect(grp, 'the list drew its groups').not.toBeNull();
    expect(grp?.querySelector('.sg-sub'), 'a group carries its sub-lines').not.toBeNull();

    // jsdom fires no enter on an ancestor for a synthetic child event, so
    // the group's own enter stands in for the entry landing on its sub-line.
    grp?.dispatchEvent(pointer('pointerenter', 'mouse'));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();

    expect(list(), 'the entry holds the panel open').not.toBeNull();
  });

  it('closes on a row pick and asks for nothing: the drive page is not built', () => {
    draw();
    toggle()?.click();
    flushSync();
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the pick closes the list').toBeNull();
    expect(get(scrollAsk), 'and asks the column for nothing').toBeNull();
  });

  it('draws two servers reading the same state without colliding', () => {
    // Rows key on the server's name, and the state text is what they share:
    // keyed by a drawn figure, this pair throws.
    draw([
      { name: 'a', k: 'a \u{b7} session', v: '2 tools', tools: [], command: null, reason: null },
      { name: 'b', k: 'b \u{b7} session', v: '2 tools', tools: [], command: null, reason: null },
    ]);
    toggle()?.click();
    flushSync();

    expect(rows(), 'both servers drew').toHaveLength(2);
  });
});

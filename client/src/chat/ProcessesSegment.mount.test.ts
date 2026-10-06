// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { scrollAsk } from '../session/scroll-ask';
import type { BackgroundTask, ProcessSnapshot } from '../session/wire';
import ProcessesSegment from './ProcessesSegment.svelte';
import { processes } from './processes.svelte';

/**
 * The segment's interaction state machine, which SSR cannot hold - the same
 * guards the agents row's own mount suite pins, because this row is its
 * sibling by construction: the hover-into-the-gap crossing, the close grace,
 * focus opening and Escape closing, the tap whose synthesised enter must not
 * eat its click, and the rows' own focus and Escape, so the panel a keyboard
 * opened can be closed the same way.
 */
let app: Record<string, unknown> | null = null;

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  processes.sync(null, [], false);
  scrollAsk.set(null);
  vi.useRealTimers();
});

beforeEach(() => {
  vi.useFakeTimers();
});

const walk = (tick: number, memory = 512): ProcessSnapshot => ({
  processes: [
    {
      pid: 8842,
      parent_pid: 1,
      name: 'cargo',
      command: 'cargo nextest run -p forge-web',
      memory_bytes: memory,
    },
  ],
  scanned_at: { secs_since_epoch: tick, nanos_since_epoch: 0 },
});

const registryRow = (over: Partial<BackgroundTask> = {}): BackgroundTask => ({
  task_id: 't-1',
  task_type: 'local_bash',
  description: 'Run the full suite',
  command: 'cargo nextest run',
  tool_use_id: 'tu-1',
  ...over,
});

function draw(rows: BackgroundTask[] = [registryRow()], tick = 1) {
  processes.sync(walk(tick), rows, true);
  app = mount(ProcessesSegment, { target: document.body });
  flushSync();
}

const toggle = () => document.querySelector<HTMLButtonElement>('.sg-tog');
const list = () => document.querySelector('.sg-list');
const rows = () => [...document.querySelectorAll<HTMLButtonElement>('.sg-it')];

/** A pointer event of the kind the browser synthesises, with its device said. */
function pointer(type: string, pointerType: string): PointerEvent {
  return new PointerEvent(type, { pointerType, bubbles: false });
}

describe("the processes row's interaction state machine", () => {
  it('reads a tap as open: the synthesised enter must not eat the click', () => {
    draw();
    toggle()?.dispatchEvent(pointer('pointerenter', 'touch'));
    toggle()?.click();
    flushSync();

    expect(list(), 'the first tap leaves the list open').not.toBeNull();
  });

  it('reads a tap as open when the press itself focuses the toggle first', () => {
    // Chromium's measured tap order: pointerdown/up complete first, then the
    // compat mousedown, the focus it causes, and the click.
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
  });

  it('keeps the list while focus moves inside it, and closes on a row Escape', () => {
    draw();
    toggle()?.click();
    flushSync();
    const row = rows()[0];
    expect(row, 'the list drew its row').not.toBeUndefined();
    if (row === undefined) return;

    // Tabbing into a row is a crossing, not a leave: the panel stays.
    row.focus();
    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: row }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'focus inside keeps it').not.toBeNull();

    // And the panel a keyboard opened is closable from where the keyboard is,
    // which is the defect a focus latch would be.
    row.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    expect(list(), 'Escape on a row closes it').toBeNull();
    expect(document.activeElement, 'and focus returns to the toggle').toBe(toggle());
  });

  it('closes when focus leaves a row without landing inside the segment', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    const row = rows()[0];
    row?.focus();

    // A Tab that leaves the segment: the row's own focusout arms the close,
    // which is what keeps a Tab-through from latching the panel open.
    row?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: document.body }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'a focus that left the rows closes it').toBeNull();
  });

  it('tags a call the walk missed with its kind', () => {
    draw([registryRow({ tool_use_id: null, command: 'setsid tail -f /gone.log' })]);
    toggle()?.click();
    flushSync();

    expect(
      rows()[0]?.textContent,
      'the missed row wears its kind where a figure would sit',
    ).toContain('local_bash');
  });

  it('closes the panel when focus leaves the segment entirely', () => {
    draw();
    toggle()?.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
    flushSync();
    expect(list(), 'focus opened it').not.toBeNull();

    toggle()?.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: null }));
    flushSync();
    vi.advanceTimersByTime(150);
    flushSync();
    expect(list(), 'a focus that left the segment closes it').toBeNull();
  });

  it('holds its order while open, and re-sorts once shut', () => {
    const pair = [
      registryRow({ task_id: 't-1', description: 'Suite', command: 'cargo nextest run' }),
      registryRow({ task_id: 't-2', description: 'Watch', command: 'tail -f watch.log' }),
    ];
    const seen = (suite: number, watch: number, tick: number): ProcessSnapshot => ({
      processes: [
        {
          pid: 8842,
          parent_pid: 1,
          name: 'cargo',
          command: 'cargo nextest run -p forge-web',
          memory_bytes: suite,
        },
        {
          pid: 8841,
          parent_pid: 1,
          name: 'tail',
          command: 'tail -f watch.log',
          memory_bytes: watch,
        },
      ],
      scanned_at: { secs_since_epoch: tick, nanos_since_epoch: 0 },
    });
    processes.sync(seen(512, 64, 1), pair, true);
    app = mount(ProcessesSegment, { target: document.body });
    flushSync();
    toggle()?.click();
    flushSync();
    expect(rows()[0]?.textContent, 'biggest first on open').toContain('Suite');

    // The walk swaps them underneath: a live order would re-shuffle the row
    // under the pointer, so the open list holds what it showed.
    processes.sync(seen(64, 512, 2), pair, true);
    flushSync();
    expect(rows()[0]?.textContent, 'the open list keeps its held order').toContain('Suite');

    // Shut and reopened, it takes the new order - held is per-open, not stuck.
    toggle()?.click();
    flushSync();
    toggle()?.click();
    flushSync();
    expect(rows()[0]?.textContent, 'a fresh open takes the live order').toContain('Watch');
  });

  it('asks to reveal the call on a pick, and closes', () => {
    draw();
    toggle()?.click();
    flushSync();
    expect(
      rows()[0]?.textContent,
      'the row figure joins its parts with spaced separators',
    ).toContain('512 B \u{b7} pid 8842');
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the panel closes on a pick').toBeNull();
    expect(get(scrollAsk), 'the row asked the column to reveal its call').toMatchObject({
      what: 'dispatch',
      call: 'tu-1',
    });
  });

  it('closes without asking when the row has no call to reach', () => {
    draw([registryRow({ tool_use_id: null })]);
    toggle()?.click();
    flushSync();
    rows()[0]?.click();
    flushSync();

    expect(list(), 'the panel closes').toBeNull();
    expect(get(scrollAsk), 'and nothing is asked that cannot land').toBeNull();
  });
});

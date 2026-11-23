// @vitest-environment jsdom
/**
 * A turn taller than the column, as the list is handed it.
 *
 * **The defect this pins is one item holding a whole giant turn** (#3035): the
 * list virtualises ITEMS, so a turn drawn as one item is laid out in full
 * whenever any part of it is on screen - tens of thousands of pixels of DOM
 * built in one frame, and the black a reader meets scrolling up through it.
 * The turn must reach the list as several items, one per slice of its rows, so
 * the list can draw the slice a reader is looking at and leave the rest data.
 *
 * jsdom draws no pixels, so what is pinned is the shape the list is handed and
 * what each item draws: the rows of the giant turn reach the page, split into
 * items that can be drawn independently, and no item is left hidden - which is
 * virtua's own black (#2398) and the column's one guard against it.
 */
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';

vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

const { default: Chat } = await import('./Chat.svelte');
const { installResizeObserver } = await import('./testing/viewport');
const { moves, records } = await import('./testing/records');
const { PART_ROWS } = await import('./rows');
const { scrollAsk } = await import('../session/scroll-ask');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** A connection the test drives by hand, answering only what a mount asks. */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: () => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (_fn: (status: ConnectionStatus) => void) => () => undefined,
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;
  return {
    connection,
    page(turns: unknown[]): void {
      for (const fn of listeners) {
        fn({ kind: 'page', conversation: LEAD, turns, cursor: null });
      }
      flushSync();
    },
  };
}

/** The reader's own frame, which is the one kind a turn opens at. */
const prompting = (text: string): unknown => ({
  type: 'user',
  uuid: `u-${text}`,
  message: { role: 'user', content: [{ type: 'text', text }] },
});

/** One call and its answer, which draw as one row of the turn's work. */
const call = (at: number): unknown[] => [
  {
    type: 'assistant',
    uuid: `a-${at}`,
    timestamp: '2026-10-01T06:00:01Z',
    message: {
      id: `m-${at}`,
      role: 'assistant',
      model: 'claude-opus-5',
      content: [
        {
          type: 'tool_use',
          id: `toolu_${at}`,
          name: 'Read',
          input: { file_path: `src/file-${at}.rs` },
        },
      ],
    },
  },
  {
    type: 'user',
    uuid: `r-${at}`,
    message: {
      role: 'user',
      content: [{ type: 'tool_result', tool_use_id: `toolu_${at}`, content: `file ${at}` }],
    },
  },
];

/**
 * How many calls the giant turn holds, which is what its items are cut from.
 *
 * **Not a multiple of the part size**, so the last slice is a remainder: a
 * slicer that dropped a tail would lose its rows silently, and a multiple
 * fixture would never draw one.
 */
const CALLS = 241;

/** One turn of `CALLS` answered calls: the oversized turn a seat can open to. */
function giant(): unknown[] {
  return [prompting('go'), ...Array.from({ length: CALLS }, (_, at) => call(at)).flat()];
}

let app: Record<string, unknown> | null = null;

afterEach(() => {
  if (app !== null) void unmount(app);
  app = null;
  document.body.innerHTML = '';
  records.length = 0;
  moves.length = 0;
  scrollAsk.set(null);
});

describe('a turn too tall to draw as one item', () => {
  it('reaches the list as slices of its rows, none of them hidden', () => {
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([
      { key: 't-big', messages: giant() },
      { key: 't-small', messages: [prompting('and a small one')] },
    ]);

    const handed = records.at(-1)?.length ?? 0;
    expect(handed, 'the giant turn is not one item').toBeGreaterThan(2);
    // The reader's own words, then the calls' one run cut into slices, then
    // the small turn: exactly what the model says, not a guess about a count.
    expect(handed, "the prompt, the run's slices, then the small turn").toBe(
      1 + Math.ceil(CALLS / PART_ROWS) + 1,
    );

    // `[data-t]` rather than `.turn`: the test list's own item wrapper wears
    // that class too, and the real one does not.
    const items = [...document.querySelectorAll('.conv [data-t]')];
    expect(items.length, 'one wrapper per item').toBe(handed);
    for (const row of items) {
      // The list's own item wrapper is the parent, which is where virtua hides
      // an unmeasured item - the black (#2398). The column's style has to win
      // on every item, the sliced ones included.
      expect(
        row.parentElement?.style.visibility,
        'a row the list has not measured is still drawn',
      ).toBe('visible');
    }

    // **The rows themselves reach the page**: every call the turn holds draws
    // a row, in order, across the items that carry them.
    const rows = [...document.querySelectorAll('.conv details.leaf')];
    expect(rows.length, 'every row of the turn is drawn').toBe(CALLS);

    // **And the first and last rows are in different items**, which is what
    // lets the list draw one without the other - the whole of the fix.
    const first = rows[0]?.closest('[data-t]');
    const last = rows.at(-1)?.closest('[data-t]');
    expect(first, "the turn's head is drawn").not.toBeNull();
    expect(last, 'and its foot').not.toBeNull();
    expect(first, 'the head and the foot are not one item').not.toBe(last);
    expect(first?.getAttribute('data-t'), 'both belong to the one turn').toBe(
      last?.getAttribute('data-t'),
    );
    expect(first?.hasAttribute('data-tend'), 'the head does not end the turn').toBe(false);
    expect(last?.hasAttribute('data-tend'), 'the foot does').toBe(true);
    expect(
      items.filter((item) => item.hasAttribute('data-tend')).length,
      'one item ends the turn',
    ).toBe(2); // the giant turn's foot and the whole small turn
  });

  it('draws a slice with only the units it was handed', () => {
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([{ key: 't-big', messages: giant() }]);

    const items = [...document.querySelectorAll('.conv [data-t]')];
    expect(items.length, 'the giant turn is several items').toBe(1 + Math.ceil(CALLS / PART_ROWS));
    const counts = items.map((item) => item.querySelectorAll('details.leaf').length);
    // The first item is the reader's own words; the rest are the run's slices,
    // every one of them a full slice but the last: no item is drawn with the
    // whole turn, which is the cost the split exists to remove.
    expect(counts[0], "the first item draws the reader's words").toBe(0);
    for (const [at, count] of counts.slice(1).entries()) {
      const wanted = Math.min(PART_ROWS, CALLS - at * PART_ROWS);
      expect(count, `slice ${at} draws its own rows`).toBe(wanted);
    }
    // **The final slice is the remainder and is drawn**: a tail dropped here
    // would be a row missing from the transcript with nothing saying so.
    expect(counts.at(-1), 'the last slice draws the tail').toBe(CALLS % PART_ROWS);
    expect(Math.max(...counts), 'no item draws the whole turn').toBeLessThan(CALLS);
  });

  it('sends a dispatch jump to the item that draws its call', () => {
    const server = stub();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([{ key: 't-big', messages: giant() }]);
    moves.length = 0;

    // A call deep in the run: its item is several slices below the turn's
    // head, which is where a jump that could not name it would land.
    const wanted = 1 + Math.floor(200 / PART_ROWS);
    scrollAsk.set({ what: 'dispatch', token: 1, call: 'toolu_200' });
    flushSync();

    expect(moves.at(-1)?.at, 'the jump lands on the item that draws the call').toBe(wanted);
    const items = [...document.querySelectorAll('.conv [data-t]')];
    const row = document.querySelector('.conv details.leaf[data-k="call-c-toolu_200"]');
    expect(row, 'the call drew').not.toBeNull();
    expect(row?.closest('[data-t]'), 'and it is the item the jump asked for').toBe(items[wanted]);
  });
});

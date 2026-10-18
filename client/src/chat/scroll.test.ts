// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * The list is a stub, because what has to be seen is what the column HANDS it.
 *
 * `virtua` reads the compensation once, as the row count changes, and consults
 * it nowhere else - so the difference between armed and not is one frame, and
 * no browser measurement holds a frame. The stub reads it in the same place
 * virtua does, an effect that runs before the DOM update.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

// The column's record is frozen where it is handed out, so a write into any
// part of it - the prepend path's composed turns included - throws here too.
vi.mock('./conversation', async (importOriginal) => {
  const { frozenConversation } = await import('./testing/frozen');
  return frozenConversation(await importOriginal<typeof import('./conversation')>());
});

const { clear, firstGrowth, last, list, setMeasured } = await import('./testing/records');
const { default: Chat } = await import('./Chat.svelte');
const { scrollAsk } = await import('../session/scroll-ask');
const { installResizeObserver, resized } = await import('./testing/viewport');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

/** One turn as a page carries it. */
const turn = (key: string): unknown => ({
  key,
  messages: [{ type: 'user', uuid: `u-${key}`, message: { role: 'user', content: [] } }],
});

/**
 * A connection a test drives by hand, with a switch for whether it sends.
 *
 * `more` answering `false` is what a socket that is not open does, and the
 * chat is expected to have asked nothing when it happens.
 */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const asks: ClientMessage[] = [];
  let open = true;
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (conversation: SessionSlot, before: string | null, turns: number) => {
      if (!open) return false;
      asks.push({ kind: 'more', conversation, before, turns });
      return true;
    },
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: () => () => undefined,
    store: () => undefined,
    settings: () => null,
    browserRole: () => false,
    onBrowserRole: () => () => undefined,
    onProfilesChanged: () => () => undefined,
    status: () => (open ? ('open' as const) : ('closed' as const)),
    close: () => undefined,
  } as unknown as Connection;

  return {
    connection,
    asks,
    shut: () => {
      open = false;
    },
    page(turns: unknown[], cursor: string | null): void {
      for (const fn of listeners) {
        fn({ kind: 'page', conversation: LEAD, turns, cursor });
      }
      flushSync();
    },
    /** One session update for this seat, as the socket delivers it. */
    update(update: unknown): void {
      for (const fn of listeners) {
        fn({ kind: 'update', update } as ServerMessage);
      }
      flushSync();
    },
    /** The core refusing the ask it took: no page is coming for it. */
    refuse(): void {
      for (const fn of listeners) {
        fn({
          kind: 'error',
          what: 'more',
          why: 'the conversation is not held yet',
        });
      }
      flushSync();
    },
    /** One turn arriving below the reader, the way a live turn does. */
    append(): void {
      for (const fn of listeners) {
        fn({
          kind: 'update',
          update: {
            chat_appended: {
              key: LEAD,
              msg: { type: 'assistant', uuid: 'live', message: { content: [] } },
            },
          },
        });
      }
      flushSync();
    },
  };
}

let app: Record<string, unknown> | null = null;

function draw(server: ReturnType<typeof stub>): void {
  clear();
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection },
  });
  flushSync();
}

/** Let the tick a settling compensation waits on run. */
async function tick(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  flushSync();
}

/** Let a stream frame's own publish land: the chat draws it on a frame (#1670). */
async function frame(): Promise<void> {
  await new Promise((resolve) => requestAnimationFrame(() => resolve(undefined)));
  flushSync();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
  // A walk's ask outlives the column it was set for, so a test that left one
  // standing would move the next test's column.
  scrollAsk.set(null);
});

beforeEach(() => {
  clear();
});

describe('the compensation the list is handed', () => {
  it('keeps a pulled prompt row mounted at its height, so a send cannot flash it', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], null);
    await frame();

    // The row arrives before the pile hears about the queue, opening the live
    // turn - the race the retraction exists for (#1890).
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'p1',
          message: { role: 'user', content: [{ type: 'text', text: 'the words' }] },
        },
      },
    });
    // The chat publishes on a frame (#1670), so the update reaches the DOM one
    // RAF later - without this the reads below are of a stale tree.
    await frame();
    const rows = [...document.querySelectorAll('.turn .turn')];
    const row = rows[rows.length - 1];
    expect(row, 'the words opened a row').toBeDefined();

    // The queue pulls the words into the card. The ROW stays the same element
    // - removed, the list splices its measured size away and the drain's
    // re-add draws it on the estimate's offset for one frame.
    server.update({ prompt_queued: { key: LEAD, uuid: 'p1', source: 'you', text: 'the words' } });
    await frame();
    const after = [...document.querySelectorAll('.turn .turn')];
    expect(after[after.length - 1], 'the row is the same element').toBe(row);
  });

  it('is armed before an older page arrives, and off again after it lands', async () => {
    const server = stub();
    draw(server);
    // Two turns, so the fresh open's fill has nothing of its own to ask for
    // and the reader's own reach is the ask this test is about.
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // The reader reaches the top, which asks for what is above. The
    // compensation has to be on for the rows that answer is about to add: it
    // is applied AS they change, so a page that armed it when the answer
    // arrived would be arming after the change it exists for.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'the ask went').toHaveLength(2);

    expect(last(), 'armed as the ask goes out').toEqual({ length: 2, shift: true });

    server.page([turn('t0'), turn('t2')], null);
    const grew = firstGrowth();
    expect(grew, 'the list gained a row').toBeDefined();
    expect(grew?.shift, 'with the compensation already on').toBe(true);

    await tick();
    expect(last()?.shift, 'and off again once the list has taken it').toBe(false);
  });

  it('is not armed by an ask the socket refused', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    server.shut();
    list()?.scrolledTo(0, 500, 320);
    flushSync();

    // Nothing was sent, so no page is coming and there is nothing to
    // compensate for. Armed here, the flag stays on: the next turn appended
    // below the reader goes through the prepend path, which moves them AND
    // leaves the list's measured sizes attributed to the wrong rows.
    expect(last()?.shift, 'an ask that never went arms nothing').toBe(false);

    server.append();
    expect(last()?.shift, 'and the turn that arrives below it is not compensated').toBe(false);
  });

  it('is held while a second ask is still in flight', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    list()?.scrolledTo(0, 500, 320);
    flushSync();
    server.page([turn('t1'), turn('t2')], '1');
    // The compensation is applied by scrolling the list, which fires a scroll
    // event of its own - so a second ask inside this window is the ordinary
    // case, not a contrived one.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'two asks are out').toHaveLength(3);
    expect(last()?.shift, 'still armed for the second').toBe(true);

    await tick();
    expect(last()?.shift, 'and one landing does not disarm the other').toBe(true);

    server.page([turn('t0'), turn('t1')], null);
    await tick();
    expect(last()?.shift, 'both landed, so off').toBe(false);
  });
});

describe('the page a height-bounded ask requests', () => {
  it('sizes the count from the measured turns and the room, not from a constant', async () => {
    const server = stub();
    draw(server);
    // The column starts at the foot of a history taller than the reach, so
    // the opening pin's own echo does not ask; the ask under test is the
    // reader's.
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // jsdom reports no layout, so the observer hands every row its height and
    // the column's own median becomes the size a page is built from.
    resized(100);
    list()?.scrolledTo(0, 500, 320);
    flushSync();

    // A screen and a half (1.5 x 320) of 100 px turns is five, where the
    // protocol's fixed page would have asked for twenty.
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    const lastAsk = asks[asks.length - 1];
    expect(lastAsk?.turns, 'the ask carries the measured page').toBe(5);
  });

  it('never asks for more than the client page ceiling, however short the turns are', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // A 20 px turn in a 320 px viewport wants twenty-four; the ask's own
    // clamp is the one line that holds it to the client page.
    resized(20);
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.turns, 'clamped to the client page').toBe(20);
  });

  it('asks for one turn when a turn is taller than twice the target', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // A 1000 px turn in a 320 px viewport: a screen and a half of it rounds
    // to zero, and the ask's own floor is what makes it a page of one. An
    // 800 px turn would round to one by itself and pin nothing.
    resized(1000);
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.turns, 'the floor is one turn').toBe(1);
  });

  it('asks for the protocol page before any turn has measured', async () => {
    const server = stub();
    draw(server);
    // The column starts at the foot of a history taller than the reach, and
    // NO row has measured: the median is undefined, so the reader's first
    // pull is the protocol's own page rather than a computed one.
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    list()?.scrolledTo(0, 500, 320);
    flushSync();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.turns, 'nothing measured, the default page').toBe(20);
  });

  it('asks for the protocol page when the column has no measured room', async () => {
    const server = stub();
    draw(server);
    // A viewport that reports no height: a page cannot be sized from a room
    // nobody has, so the fallback answers instead of dividing by zero.
    setMeasured(900, 0);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    resized(100);

    list()?.scrolledTo(0, 500, 0);
    flushSync();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.turns, 'no room, the default page').toBe(20);
  });

  it("carries the measured page on the pin's own echo", async () => {
    const server = stub();
    draw(server);
    // A history whose foot sits inside the reach: the landing's own pin
    // writes the foot, and the echo of that write is the event this test
    // reads. The landing's echo asks before anything measured, so it takes
    // the default page and is answered first.
    setMeasured(500, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    server.page([turn('t0')], '1');
    await tick();

    // A settled move to the top that fires no event, then a size change: the
    // column's watcher pins the foot from 0, and the pin's echo carries the
    // reach ask.
    list()?.settled(0, 500, 320);
    resized(100);
    await tick();

    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.turns, 'the echo asks the measured page').toBe(5);
  });
});

describe('the page a walk requests', () => {
  it('keeps the protocol page for the compaction walk', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    // A median that would size the reader's own pull at five, so a walk that
    // read it would ask for five.
    resized(100);

    scrollAsk.set({ what: 'compaction', token: 1 });
    flushSync();
    await tick();

    const asks = server.asks.filter((ask) => ask.kind === 'more');
    const lastAsk = asks[asks.length - 1];
    expect(lastAsk?.before, 'the walk asked above').toBe('2');
    expect(lastAsk?.turns, 'the walk keeps the protocol page, not the measured one').toBe(20);

    // The walk's FIRST ask is the column's own, so it arms the line; the
    // pages the walk asks on from there are the conversation's, which arm
    // nothing, and the page that ends the history settles the line.
    expect(
      document.querySelector('.older-loading'),
      'the walk asks through the same flight',
    ).not.toBeNull();
    server.page([turn('t0')], null);
    await tick();
    expect(document.querySelector('.older-loading'), 'the page settled it').toBeNull();
  });

  it('keeps the protocol page for the dispatch chase', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    resized(100);

    // A call no loaded turn carries: the chase walks the history for it.
    scrollAsk.set({ what: 'dispatch', token: 1, call: 'tu-nowhere' });
    flushSync();
    await tick();

    const asks = server.asks.filter((ask) => ask.kind === 'more');
    const lastAsk = asks[asks.length - 1];
    expect(lastAsk?.before, 'the chase walked above').toBe('2');
    expect(lastAsk?.turns, 'the chase keeps the protocol page, not the measured one').toBe(20);
  });
});

describe('the history above a column that fits', () => {
  it('asks for the page above, because there is no scroll to reach the top with', async () => {
    const server = stub();
    draw(server);
    // The whole newest page fits the room: the list cannot scroll, so no
    // reach ask can ever ride a scroll event (#2139).
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    // The rows report, and the deferred check reads the settled layout.
    resized(100);
    await frame();
    await tick();

    const asks = server.asks.filter((ask) => ask.kind === 'more');
    const lastAsk = asks[asks.length - 1];
    expect(lastAsk?.before, 'the fitting column asked above').toBe('2');
    expect(lastAsk?.turns, 'and with the measured page').toBe(5);
  });

  it('leaves a window that overflows to the scroll', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const before = server.asks.length;
    resized(100);
    await frame();
    await tick();
    expect(server.asks.length, 'nothing to fill: the column already scrolls').toBe(before);
  });

  it('asks nothing when the history has no page above', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], null);
    await tick();
    resized(100);
    await frame();
    await tick();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks, 'the first page is all there is').toHaveLength(1);
  });

  it('asks nothing before the list has a layout to read', async () => {
    const server = stub();
    draw(server);
    // An unmeasured list reports no height at all, which reads as fitting. A
    // fill asked on that takes a page the column does not need.
    setMeasured(0, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    resized(100);
    await frame();
    await tick();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks, 'no layout, no fit to trust').toHaveLength(1);
  });

  it('waits for every held turn to measure before it trusts the fit', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    // No row has reported a height: a list still drawing reads short, and a
    // fill asked on that would take a page it does not need.
    await frame();
    expect(server.asks, 'nothing measured, nothing asked').toHaveLength(1);

    // Two deliveries, as a browser gives two: the first measures the rows,
    // the second is the batch after they reported, where the fit is known.
    resized(100);
    resized(100);
    await tick();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks.length, 'measured and fitting, the page above is asked').toBe(2);
  });
});

describe('the loading state over the column', () => {
  it('says an older page is loading while the ask is out, and stops when it lands', async () => {
    const server = stub();
    draw(server);
    // The opening pin lands at the foot of a history taller than the reach,
    // so its own echo asks nothing and the only ask is the reader's.
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    expect(document.querySelector('.older-loading'), 'nothing in flight yet').toBeNull();

    list()?.scrolledTo(0, 900, 320);
    flushSync();
    expect(
      document.querySelector('.older-loading')?.textContent,
      'the clamped top says what it is waiting for',
    ).toContain('Loading older messages');

    server.page([turn('t0'), turn('t1')], null);
    await tick();
    expect(document.querySelector('.older-loading'), 'the page landed').toBeNull();
  });

  it('is not shown for an ask the socket refused, which nothing is coming for', async () => {
    const server = stub();
    draw(server);
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const asks = server.asks.length;

    server.shut();
    list()?.scrolledTo(0, 900, 320);
    flushSync();
    expect(server.asks.length, 'the refusal never left: no ask went').toBe(asks);
    expect(document.querySelector('.older-loading'), 'no ask went, no state').toBeNull();
  });

  it('goes when the ask is refused after it went out, which is a dropped ask', async () => {
    const server = stub();
    draw(server);
    // A history taller than the reach, so the opening pin asks nothing and the
    // only ask in flight is the reader's.
    setMeasured(900, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    list()?.scrolledTo(0, 900, 320);
    flushSync();
    expect(document.querySelector('.older-loading'), 'the ask is out').not.toBeNull();

    // The core refuses the ask it took: no page is coming, and the line must
    // not outlive it.
    server.refuse();
    await tick();
    expect(document.querySelector('.older-loading'), 'the refusal settled it').toBeNull();
  });
});

describe('a row the list has not measured', () => {
  it('is drawn rather than left hidden, so a fling is not a black column', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1')], null);
    await tick();

    // The item wrapper is the element the list places; virtua's own styling
    // lives on it, so that is where an item attribute has to land.
    const row = document.querySelector<HTMLElement>('.conv > .turn');
    expect(row, 'the page drew a row').not.toBeNull();
    // virtua sets `visibility: hidden` on a row it has not measured; the
    // column's own item attribute rides after it, which is what paints the
    // row at its natural height on the estimate's offset instead (#2398).
    expect(row?.style.visibility, 'the unmeasured row is painted').toBe('visible');
  });
});

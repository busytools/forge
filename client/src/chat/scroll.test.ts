// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ClientMessage, ServerMessage } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
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

const { clear, firstGrowth, last, list, records, setMeasured } = await import('./testing/records');
const { default: Chat } = await import('./Chat.svelte');
const { default: SeatSwitch } = await import('./testing/seat-switch.svelte');
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
 * One handoff record, as the wire carries it, with the judgment turn a
 * catching-up client joins the conversation on.
 */
const handoffRecord = (prompt: string): unknown => ({
  slot: LEAD,
  from: 'old',
  to: 'new',
  reason: 'fill',
  fill_percent: 85,
  total_tokens: 850_000,
  max_tokens: 1_000_000,
  compaction_count: 0,
  at_percent: 85,
  brief: { ground_truth: [], judgment: [] },
  judgment_turn: [
    {
      type: 'user',
      uuid: prompt,
      message: { role: 'user', content: [{ type: 'text', text: 'A handoff is starting.' }] },
    },
  ],
  transcript_path: '/tmp/old.jsonl',
  at: '2026-10-24T08:00:00Z',
});

/**
 * A connection a test drives by hand, with a switch for whether it sends.
 *
 * `more` answering `false` is what a socket that is not open does, and the
 * chat is expected to have asked nothing when it happens.
 */
function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
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
    onStatus: (fn: (status: ConnectionStatus) => void) => {
      statuses.add(fn);
      return () => statuses.delete(fn);
    },
    store: () => undefined,
    settings: () => null,
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
    /** One page for a named seat, which a switch test draws the other column from. */
    pageFor(slot: SessionSlot, turns: unknown[], cursor: string | null): void {
      for (const fn of listeners) {
        fn({ kind: 'page', conversation: slot, turns, cursor });
      }
      flushSync();
    },
    /** The socket going down or coming back, as the connection reports it. */
    reach(status: ConnectionStatus): void {
      open = status === 'open';
      for (const fn of statuses) fn(status);
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
    /**
     * Messages delivered in ONE task with no paint between them, which is the
     * burst a socket produces and the order every boundary pin is about.
     */
    burst(...messages: ServerMessage[]): void {
      for (const fn of listeners) for (const message of messages) fn(message);
      flushSync();
    },
    /**
     * The seat's own snapshot, which is the one frame that says whether a turn
     * is running - and so the one that lets a turn's ending mean anything - and
     * the carrier of a handoff record a catching-up client joins on.
     */
    snapshot(turns: unknown[], running: boolean, handoff: unknown = null): void {
      for (const fn of listeners) {
        fn({
          kind: 'snapshot',
          subject: { session: LEAD },
          data: {
            header: { turn_in_flight: running },
            conversation: { turns, cursor: null, compaction_count: 0, handoff },
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

/** The older asks one seat was sent, in the order they went. */
type MoreAsk = Extract<ClientMessage, { kind: 'more' }>;

function asksFor(server: ReturnType<typeof stub>, slot: SessionSlot): MoreAsk[] {
  return server.asks.filter(
    (ask): ask is MoreAsk => ask.kind === 'more' && ask.conversation.label === slot.label,
  );
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

/**
 * Let every deferred fill pass run: the column's own frame, then the tick.
 *
 * The fill schedules a frame of its own, so a test that read around it would
 * race the frame's delivery - under load the frame can land between any two
 * awaits. Every fill test reads only after this.
 */
async function settleFrames(): Promise<void> {
  await frame();
  await tick();
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

  it('compensates the landing that moved the head', async () => {
    const server = stub();
    draw(server);
    // Two turns, so the fresh open's fill has nothing of its own to ask for
    // and the reader's own reach is the ask this test is about.
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // The list built from nothing is not a head move: there was no row above
    // the reader to hold, so filling an empty one is a plain growth.
    expect(records[0], 'the first page is not a prepend').toEqual({ length: 2, shift: false });

    // The reader reaches the top, which asks for what is above. The ask is
    // not a change to the list, so nothing is armed for it: the compensation
    // belongs to the landing, which is the change that moves the head.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'the ask went').toHaveLength(1);

    server.page([turn('t0'), turn('t2')], null);
    const grew = firstGrowth();
    expect(grew, 'the list gained a row').toBeDefined();
    expect(grew?.shift, 'and the change that moved the head is compensated').toBe(true);
  });

  it('does not compensate a turn that opens at the tail', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // A page is on its way, which is the window the compensation used to be
    // armed for the whole of: a turn opening at the tail inside it went
    // through the prepend path, and every row below landed on the row above's
    // offset until it re-measured (Ved, 2026-10-28: two rows in one band).
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'the page is in flight').toHaveLength(1);

    // A prompt that opens a turn of its own below the reader - the arrival
    // that made this a defect: a cron's words landing while a page was read.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-tail',
          message: { role: 'user', content: [{ type: 'text', text: 'the chore' }] },
        },
      },
    });
    await frame();
    expect(last()?.length, 'the prompt opened a row').toBe(3);
    expect(last()?.shift, 'a turn arriving below the reader is not a prepend').toBe(false);
  });

  it('does not compensate a page that grows the tail', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // **An ask in flight, which is what the old flag read.** The pin has to be
    // armed here or it cannot tell this fix from the window it replaced: with
    // nothing outstanding the moment flag was false anyway.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'the page is in flight').toHaveLength(1);

    // A catch-up or reconnect page: the newest read repeats the held tail and
    // carries one more turn below it. Nothing moved at the head, so the rows
    // above the reader are where they were and the new row is a plain append.
    server.page([turn('t1'), turn('t2'), turn('t3')], null);
    await tick();
    expect(last()?.length, 'the newest page grew the tail').toBe(3);
    expect(last()?.shift, 'a page that grows the tail is not a prepend').toBe(false);
  });

  it('draws a pending frame and a landing as two changes', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // **The order a socket burst produces.** A frame opens a turn at the tail
    // and the page lands before the frame's paint; one merged update would
    // carry the page's rows above and the frame's row below as a single
    // growth, and the list would compensate the whole net delta as a prepend.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-burst',
          message: { role: 'user', content: [{ type: 'text', text: 'the burst' }] },
        },
      },
    });
    server.page([turn('t0'), turn('t1'), turn('t2')], null);
    await frame();

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the frame, then the landing',
    ).toEqual([
      [3, false],
      [4, true],
    ]);
  });

  it('spends the landing on the paint, not on the next frame that meets it', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // **Three writes, and the landing's paint still in the future.** The
    // helpers each render before returning, so the first two are separate
    // tasks; the third lands in the same task as the landing's deferred draw,
    // which is the window that matters. (All three in one task coalesce in
    // Svelte before any render, so the boundary there is the hidden pin's.)
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-first',
          message: { role: 'user', content: [{ type: 'text', text: 'the burst' }] },
        },
      },
    });
    server.page([turn('t0'), turn('t1'), turn('t2')], null);
    // A catch-up page one turn longer: an appended frame that opens a row is
    // held as `unturned` once a page's row is the newest, so a page is the
    // write the harness can build that still moves the list.
    server.page([turn('t0'), turn('t1'), turn('t2'), turn('t3')], null);
    await frame();

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the frame, the landing, the frame',
    ).toEqual([
      [3, false],
      [4, true],
      [5, false],
    ]);
  });

  it('keeps the landing marked when a turn ends in the same window', async () => {
    const server = stub();
    draw(server);
    // The seat's own snapshot is what says a turn is running, which is the
    // only way a turn's ending changes anything.
    server.snapshot([turn('t1'), turn('t2')], true);
    await tick();
    const handed = records.length;

    // A frame joining the running turn arms a draw; the page lands before it;
    // then the turn ends, a change that rebuilds the rows without moving the
    // head. It used to publish at once, replacing the landing's marked value
    // before its paint and handing the list one unmarked growth.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'assistant',
          uuid: 'a-open',
          message: { role: 'assistant', content: [{ type: 'text', text: 'still going' }] },
        },
      },
    });
    server.page([turn('t0'), turn('t1'), turn('t2')], null);
    server.update({ turn_cancelled: {} });
    await frame();

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the landing keeps its mark',
    ).toEqual([[3, true]]);
  });

  it('publishes a frame that spends a mark on its own tick when the page cannot paint', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // **The reverse order, hidden.** The landing comes first and the frame
    // second, both in one task; the frame spends the landing's mark, and a
    // page that cannot paint has no frame to put between the two publishes.
    // Without the tick the frame replaces the marked value and the prepend is
    // never compensated.
    const hidden = Object.getOwnPropertyDescriptor(Document.prototype, 'hidden');
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    try {
      server.burst(
        {
          kind: 'page',
          conversation: LEAD,
          turns: [turn('t0'), turn('t1'), turn('t2')],
          cursor: null,
        },
        {
          kind: 'update',
          update: {
            chat_appended: {
              key: LEAD,
              msg: {
                type: 'user',
                uuid: 'u-reverse',
                message: { role: 'user', content: [{ type: 'text', text: 'after the page' }] },
              },
            },
          },
        },
      );
      await tick();
    } finally {
      if (hidden !== undefined) Object.defineProperty(document, 'hidden', hidden);
    }

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the landing, then the frame',
    ).toEqual([
      [3, true],
      [4, false],
    ]);
  });

  it('spends a waiting mark before a frame whose turns differ', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // Frame, landing, then a frame that only joins the newest row: no row is
    // added, but the turns are rebuilt, so the landing's unpublished mark is
    // spent by that write rather than replaced by it. The first two render
    // before returning; the last lands while the landing's draw is deferred.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-head',
          message: { role: 'user', content: [{ type: 'text', text: 'the burst' }] },
        },
      },
    });
    server.page([turn('t0'), turn('t1'), turn('t2')], null);
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'assistant',
          uuid: 'a-tail',
          message: { role: 'assistant', content: [{ type: 'text', text: 'and more' }] },
        },
      },
    });
    await frame();

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the frame, then the landing marked, then the frame',
    ).toEqual([
      [3, false],
      [4, true],
    ]);
  });

  it('keeps a landing marked when a turn ends on a page that cannot paint', async () => {
    const server = stub();
    draw(server);
    // A running turn, so its ending rebuilds the rows.
    server.snapshot([turn('t1'), turn('t2')], true);
    await tick();
    const handed = records.length;

    // The landing is deferred to a tick (no frame to publish on) and the turn
    // ends before that tick. The ending does not move the head, so only the
    // unpublished mark tells the column to spend it first.
    const hidden = Object.getOwnPropertyDescriptor(Document.prototype, 'hidden');
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    try {
      server.burst(
        {
          kind: 'page',
          conversation: LEAD,
          turns: [turn('t0'), turn('t1'), turn('t2')],
          cursor: null,
        },
        { kind: 'update', update: { turn_cancelled: {} } },
      );
      await tick();
    } finally {
      if (hidden !== undefined) Object.defineProperty(document, 'hidden', hidden);
    }

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the landing keeps its mark through the ending',
    ).toEqual([[3, true]]);
  });

  // **A composition pin, not a fix pin.** It holds on the round-2 code too:
  // it says #2639's handoff row goes through the same funnel and therefore
  // gets the same boundary, not that this cut changed it.
  it('joins a mid-handoff prepend as a change of its own', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // A frame arms a draw; then the seat's record arrives carrying a handoff
    // whose judgment turn is joined AHEAD of the held rows - a client catching
    // up mid-handoff. A prepend above the reader is exactly the change that
    // must be published on its own, and #2639 put it through the same funnel.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-mid',
          message: { role: 'user', content: [{ type: 'text', text: 'while away' }] },
        },
      },
    });
    server.snapshot([turn('t1'), turn('t2')], false, handoffRecord('forge_nudge_handoff_mid'));
    await frame();

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the frame, then the handoff turn and row above it',
    ).toEqual([
      [3, false],
      [5, true],
    ]);
  });

  it('publishes a landing on its own tick when the page cannot paint', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();
    const handed = records.length;

    // **No frame is coming, so the boundary is a tick.** A hidden page flushes
    // writes at once, which puts the frame and the landing in one task and one
    // render - the merge again, by the other door.
    const hidden = Object.getOwnPropertyDescriptor(Document.prototype, 'hidden');
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    try {
      server.burst(
        {
          kind: 'update',
          update: {
            chat_appended: {
              key: LEAD,
              msg: {
                type: 'user',
                uuid: 'u-hidden',
                message: { role: 'user', content: [{ type: 'text', text: 'hidden' }] },
              },
            },
          },
        },
        {
          kind: 'page',
          conversation: LEAD,
          turns: [turn('t0'), turn('t1'), turn('t2')],
          cursor: null,
        },
      );
      await tick();
    } finally {
      if (hidden !== undefined) Object.defineProperty(document, 'hidden', hidden);
    }

    const changes = records
      .slice(handed)
      .filter((held, at, all) => at === 0 || held.length !== all[at - 1]?.length);
    expect(
      changes.map((held) => [held.length, held.shift]),
      'the frame, then the landing on its own tick',
    ).toEqual([
      [3, false],
      [4, true],
    ]);
  });

  it('compensates a second landing on its own change, and the turn after it not at all', async () => {
    const server = stub();
    draw(server);
    server.page([turn('t1'), turn('t2')], '2');
    await tick();

    // A second ask is the ordinary case: the landing's own compensation fires
    // a scroll, and a reader's reach can follow it inside the same window.
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    server.page([turn('t1'), turn('t2')], '1');
    list()?.scrolledTo(0, 500, 320);
    flushSync();
    expect(server.asks, 'two asks are out').toHaveLength(2);

    server.page([turn('t0'), turn('t1')], null);
    const grew = firstGrowth();
    expect(grew?.shift, 'the landing that moved the head is compensated').toBe(true);

    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-after',
          message: { role: 'user', content: [{ type: 'text', text: 'after the landing' }] },
        },
      },
    });
    await frame();
    expect(last()?.shift, 'and the turn opening below it after that is not').toBe(false);
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
    // The landing's deferred pass runs with nothing measured yet; then the
    // rows report over the harness's two deliveries and the settled batch
    // asks.
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();

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
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the open asked nothing - its subscribe answered').toBe(0);
    resized(100);
    resized(100);
    await settleFrames();
    expect(server.asks.length, 'nothing to fill: the column already scrolls').toBe(before);
  });

  it('asks nothing when the history has no page above', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], null);
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks, 'the first page is all there is, and it asked nothing').toHaveLength(0);
  });

  it('asks nothing before the list has a layout to read', async () => {
    const server = stub();
    draw(server);
    // An unmeasured list reports no height at all, which reads as fitting. A
    // fill asked on that takes a page the column does not need.
    setMeasured(0, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks, 'no layout, no fit to trust, and no ask').toHaveLength(0);
  });

  it('waits for every held turn to measure before it trusts the fit', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    // No row has reported a height: a list still drawing reads short, and a
    // fill asked on that would take a page it does not need. The deferred
    // pass has run by the settle.
    await settleFrames();
    expect(server.asks, 'nothing measured, nothing asked').toHaveLength(0);

    // The harness delivers one observer batch per `resized()`; a browser
    // settles its layout over more than one, so the second is that settle.
    const before = server.asks.length;
    resized(100);
    resized(100);
    await settleFrames();
    expect(server.asks.length - before, 'measured and fitting, the page above is asked').toBe(1);
  });

  it('keeps asking as pages land while the window still fits', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.before, 'the first fill went').toBe('2');

    // The page lands and the column still fits; the cursor moved, so the
    // fill asks again. A fill that stopped after one page would pass the
    // single-ask test above and fail here.
    server.page([turn('t0')], '1');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const after = server.asks.filter((ask) => ask.kind === 'more');
    expect(after[after.length - 1]?.before, 'the fill continued on the landing').toBe('1');
  });

  it('holds the fill while a landing carries a turn that has not measured', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;

    // The repeated turns are measured and the page has one NEW turn that has
    // not reported; a guard that only asked "has anything measured" would
    // ask on that.
    server.page([turn('t3')], '1');
    await settleFrames();
    expect(server.asks.length, 'the unmeasured turn holds the fill').toBe(before);

    // The harness's deliveries: the first measures the row, the second is the
    // batch after it reported.
    resized(100);
    resized(100);
    await settleFrames();
    expect(server.asks.length, 'measured, the fill asks again').toBe(before + 1);
  });

  it('does not ask the same cursor twice', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the fill asked above').toBe(1);

    // A page that repeats the cursor it was asked with is the walk's own
    // stop, and the fill stops on it too. The deliveries are the harness's
    // layout settle, so the check sees the new row measured.
    server.page([turn('t0')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    expect(server.asks.length, 'the same cursor is not asked again').toBe(before);
  });

  it('does not wait for a held row, which draws nothing and never measures', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();

    // A prompt queued and pulled into the pile leaves its row held: it draws
    // nothing and records no height until the drain, which can be minutes
    // behind a running turn. The fill is not its to wait for.
    server.update({
      chat_appended: {
        key: LEAD,
        msg: {
          type: 'user',
          uuid: 'u-held',
          message: { role: 'user', content: [{ type: 'text', text: 'wait with me' }] },
        },
      },
    });
    await frame();
    server.update({
      prompt_queued: { key: LEAD, uuid: 'u-held', source: 'you', text: 'wait with me' },
    });
    await frame();

    // The held row reports nothing, as a real zero-height row does: the two
    // deliveries measure the conversation's own turns and leave it out.
    const heldRow = (node: Element) => node.getAttribute('data-t')?.includes('u-held') === true;
    resized(100, heldRow);
    resized(100, heldRow);
    await settleFrames();
    const asks = server.asks.filter((ask) => ask.kind === 'more');
    expect(asks[asks.length - 1]?.before, 'the measured turns alone fill the room').toBe('2');
  });

  it('asks nothing more once the conversation is refused', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the fill asked above').toBe(1);

    // A landing with a NEW cursor whose row is a repeat - so nothing new
    // measures - then the refusal in the same turn, then a size change: the
    // observer's fill reaches the refusal guard too, not only the frame.
    server.page([turn('t1')], '1');
    server.refuse();
    await settleFrames();
    resized(100);
    await settleFrames();
    expect(server.asks.length, 'a refusal is retried by its own beat, not the fill').toBe(before);
  });

  it("waits an empty page's beat out and asks the same cursor again", async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw(server);
      setMeasured(300, 320);
      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      resized(100);
      flushSync();
      const before = server.asks.length;
      expect(before, 'the fill asked above').toBe(1);

      // The empty page keeps the cursor and stamps the beat; a quiet seat
      // publishes nothing else, so the fill schedules the retry itself.
      server.page([], null);
      flushSync();
      expect(server.asks.length, 'the beat has not passed yet').toBe(before);

      await vi.advanceTimersByTimeAsync(2_100);
      flushSync();
      expect(server.asks.length, 'the beat passed, the same cursor is asked again').toBe(
        before + 1,
      );
      const retried = server.asks[server.asks.length - 1];
      expect(
        retried?.kind === 'more' ? retried.before : undefined,
        'the same cursor is the one asked',
      ).toBe('2');
    } finally {
      vi.useRealTimers();
    }
  });

  it('asks the cursor again when its retry heals the refusal', async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw(server);
      setMeasured(300, 320);
      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      resized(100);
      flushSync();
      expect(server.asks.length, 'the fill asked above').toBe(1);

      server.refuse();
      flushSync();
      // The retry's beat, then the newest page that heals the refusal: the
      // retry keeps the walk cursor, so the mark must not survive it (#2411).
      await vi.advanceTimersByTimeAsync(2_100);
      flushSync();
      expect(server.asks[server.asks.length - 1], 'the retry asks the newest page').toEqual(
        expect.objectContaining({ before: null }),
      );

      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      flushSync();
      expect(server.asks[server.asks.length - 1], 'the healed conversation fills again').toEqual(
        expect.objectContaining({ before: '2' }),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it('starts the fill from nothing when the occupant is swapped', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const askedBefore = server.asks.filter(
      (ask) => ask.kind === 'more' && ask.before === '2',
    ).length;
    expect(askedBefore, 'the fill asked the cursor once').toBe(1);

    // Settle the in-flight fill ask on a page that overflows, so the swap
    // below meets an idle conversation rather than an abandoned ask.
    setMeasured(900, 320);
    server.page([turn('t0')], '1');
    await settleFrames();

    // A `/resume` or a crash recovery on the same seat: the conversation goes
    // back to nothing and loads again with a first-page cursor that spells
    // the old mark. The mark belongs to the occupant that is gone.
    setMeasured(300, 320);
    server.update({ session_replaced: {} });
    await settleFrames();
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const askedAfter = server.asks.filter(
      (ask) => ask.kind === 'more' && ask.before === '2',
    ).length;
    expect(askedAfter, 'the new occupant fills on the same cursor').toBe(askedBefore + 1);
  });

  it('asks again when a landing clears the beat without answering it', async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw(server);
      setMeasured(300, 320);
      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      resized(100);
      flushSync();
      const before = server.asks.length;
      expect(before, 'precondition: the fill asked above').toBe(1);

      // The empty page stamps the beat; then a newest page lands inside it (a
      // seat return or a reconnect) and clears the beat without answering the
      // beat's ask. The landing's own frame is the wake, before the beat.
      server.page([], null);
      flushSync();
      expect(server.asks.length, 'the beat has not passed').toBe(before);
      server.page([turn('t1'), turn('t2')], '2');
      await vi.advanceTimersByTimeAsync(34);
      flushSync();
      const last = server.asks[server.asks.length - 1];
      expect(server.asks.length, 'the cleared beat lets the fill ask again').toBe(before + 1);
      expect(last?.kind === 'more' ? last.before : null, 'and it asks the same cursor').toBe('2');

      // The cancelled beat's timer went with the landing: its own moment
      // passing asks nothing.
      await vi.advanceTimersByTimeAsync(2_100);
      flushSync();
      expect(server.asks.length, 'the cancelled beat asks nothing').toBe(before + 1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("asks the next page when the landing's rows settle at the estimate", async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the fill asked above').toBe(1);

    // The page above lands with a row that measures exactly at the estimate:
    // the list's content height does not change again, so its own watcher
    // fires no second batch. The row's own report has to be the wake.
    server.page([turn('t3')], '1');
    await settleFrames();
    resized(100);
    await settleFrames();
    const asks = asksFor(server, LEAD);
    expect(asks.length, 'the row report woke the fill').toBe(before + 1);
    expect(asks[asks.length - 1]?.before, 'and it asked the page above').toBe('1');
  });

  it('decides the fit on the layout the landing settles into, not at the landing', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the fill asked above').toBe(1);

    // The landing repeats the measured rows under a cursor above them, so
    // every guard passes at this instant - and the layout then settles into
    // an overflow before the frame runs. A fill read at the landing would
    // ask a page the settled layout does not need; the deferred check sees
    // the overflow and holds.
    server.page([turn('t1'), turn('t2')], '1');
    setMeasured(900, 320);
    await settleFrames();
    expect(server.asks.length, 'the settled overflow asked nothing').toBe(before);
  });

  it('asks the cursor a landing moved to, with no row reporting it', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    const before = server.asks.length;
    expect(before, 'precondition: the fill asked above').toBe(1);

    // A page that repeats both rows under a cursor one above them: nothing
    // new draws and no row reports a height, so the size observer has no
    // event to run on. The landing's own deferred check is the trigger.
    server.page([turn('t1'), turn('t2')], '1');
    await settleFrames();
    const asks = asksFor(server, LEAD);
    expect(asks.length, 'the landing woke the fill').toBe(2);
    expect(asks[asks.length - 1]?.before, 'for the cursor the landing moved to').toBe('1');
  });

  it('asks the cursor again after a dropped socket answers with it', async () => {
    const server = stub();
    draw(server);
    setMeasured(300, 320);
    server.page([turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    expect(asksFor(server, LEAD).at(-1)?.before, 'precondition: the fill asked above').toBe('2');
    const before = server.asks.length;

    // The socket dies with that ask in flight. The reconnect asks the newest
    // page for itself (#2420), and its answer spells the very cursor the fill
    // last asked - so the landing count is the only thing that says the fill
    // may ask it again.
    server.reach('closed');
    server.reach('open');
    expect(server.asks.length, 'the reconnect asks the newest page').toBe(before + 1);
    expect(asksFor(server, LEAD).at(-1)?.before, 'and it is a newest ask').toBeNull();
    server.pageFor(LEAD, [turn('t1'), turn('t2')], '2');
    await settleFrames();
    const asks = asksFor(server, LEAD);
    expect(asks.length, 'and the fill asks the same cursor again').toBe(before + 2);
    expect(asks[asks.length - 1]?.before, 'for the cursor the answer spells').toBe('2');
    expect(server.asks.length, 'nothing else asked').toBe(before + 2);
  });

  it('does not ask at the beat once the room has overflowed', async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw(server);
      setMeasured(300, 320);
      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      resized(100);
      flushSync();
      const before = server.asks.length;
      expect(before, 'precondition: the fill asked above').toBe(1);

      // An empty page stamps the beat; before it fires the column grows and
      // overflows. The beat wakes the fill, and the fit guard inside it - the
      // one the timer reaches without the observer's wrapper - is what keeps
      // the ask from going.
      server.page([], null);
      flushSync();
      expect(server.asks.length, 'the beat has not passed').toBe(before);
      setMeasured(900, 320);
      await vi.advanceTimersByTimeAsync(2_100);
      flushSync();
      expect(server.asks.length, 'an overflowing column asks nothing on the beat').toBe(before);
    } finally {
      vi.useRealTimers();
    }
  });

  it('asks nothing at a beat the landing cancelled, once the room grows back', async () => {
    vi.useFakeTimers();
    try {
      const server = stub();
      draw(server);
      setMeasured(300, 320);
      server.page([turn('t1'), turn('t2')], '2');
      flushSync();
      resized(100);
      resized(100);
      flushSync();
      const before = server.asks.length;
      expect(before, 'precondition: the fill asked above').toBe(1);

      // The empty page stamps the beat; a newest page lands before it and
      // clears it. The landing's own check holds - the room has grown to an
      // overflow - so the fill asks nothing and the mark stays free.
      server.page([], null);
      flushSync();
      setMeasured(900, 320);
      server.page([turn('t1'), turn('t2')], '2');
      await vi.advanceTimersByTimeAsync(34);
      flushSync();
      expect(server.asks.length, 'the landing held on the overflow').toBe(before);

      // The room comes back with no watcher batch to say so, and the old
      // beat's moment passes. A timer the landing did not cancel would run
      // the fill here, free of the mark, and ask.
      setMeasured(300, 320);
      await vi.advanceTimersByTimeAsync(2_100);
      flushSync();
      expect(server.asks.length, 'the cancelled beat asked nothing').toBe(before);
    } finally {
      vi.useRealTimers();
    }
  });

  it('fills the seat it switches to, whose cursor spells the one it left', async () => {
    const server = stub();
    clear();
    const other: SessionSlot = { ...LEAD, label: 'w1' };
    const column = mount(SeatSwitch, {
      target: document.body,
      props: { connection: server.connection, first: LEAD, second: other },
    }) as unknown as { flip: () => void };
    app = column;
    setMeasured(300, 320);
    flushSync();

    // The first seat loads and fills.
    server.pageFor(LEAD, [turn('t1'), turn('t2')], '2');
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    expect(
      asksFor(server, LEAD).map((ask) => ask.before),
      'the first seat filled, and only the fill asked - the switch itself asked no page',
    ).toEqual(['2']);

    // The second seat's conversation loads while the first is drawn, so this
    // column never ran a fill for it: it arrives held, loaded and fitting.
    column.flip();
    await settleFrames();
    column.flip();
    await settleFrames();
    server.pageFor(other, [turn('t1'), turn('t2')], '2');
    await settleFrames();

    // Back to the second seat, whose cursor spells the one the column last
    // asked on the first. The return reads nothing, and the resizes land the
    // fill on the seat now drawn: a kept, fitting seat fills on its own
    // cursor. (This is the switch's behaviour, not a pin on where the mark
    // lives - the refusal and socket tests carry that.)
    column.flip();
    await settleFrames();
    resized(100);
    resized(100);
    await settleFrames();
    expect(
      asksFor(server, other).map((ask) => ask.before),
      'the second seat filled on its own cursor, with no return read',
    ).toEqual(['2']);
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

/**
 * The one turn a settle case lands over the held window on its own: larger
 * than `HISTORY_WINDOW_BYTES`, which is what makes the landing trim.
 */
const OVER_WINDOW_MIB = 10;

/**
 * A pessimistic rate for weighing turns in jsdom on a loaded gate: the
 * over-window case below runs ~0.7 s idle and ~4 s at background QoS, and the
 * full check that reddened this file ran past vitest's five-second default.
 */
const FOLD_MIB_PER_SEC = 1;

describe(
  'the held window at the column foot',
  {
    // Weighing the window is CPU, not a wait, so there is no timer here to
    // derive a deadline from. The suite's budget is vitest's default, 5 s
    // copied here, plus its heaviest fixture at the pessimistic rate - and it
    // sits on the suite because a `vi.setConfig` call reaches only the cases
    // registered after it.
    timeout: 5_000 + (OVER_WINDOW_MIB / FOLD_MIB_PER_SEC) * 1_000,
  },
  () => {
    /** One turn heavy enough that a handful pass the window. */
    const heavy = (key: string, mb = 1): unknown => ({
      key,
      messages: [
        {
          type: 'user',
          uuid: `u-${key}`,
          message: {
            role: 'user',
            content: [{ type: 'text', text: `${key} ${'x'.repeat(Math.floor(mb * (1 << 20)))}` }],
          },
        },
      ],
    });

    /**
     * **A fitting column keeps every row**, and an overflowing one settles on the
     * newest window when the reader reaches its foot (#2532 review). Re-arming
     * the follow on a column that holds everything must not trim rows the reader
     * can see; the foot of a column that scrolls is where the oldest pages go.
     */
    it('leaves a fitting column alone and settles an overflowing one at the foot', async () => {
      const server = stub();
      draw(server);
      setMeasured(900, 320);
      server.page([turn('t0')], 'cursor-0');
      await tick();
      // The reader scrolls up and pulls pages past the window.
      list()?.scrolledTo(0, 900, 320);
      flushSync();
      for (let i = 1; i <= 8; i += 1) {
        server.page([heavy(`h${i}`)], `cursor-${i}`);
        await tick();
      }
      const full = last()?.length ?? 0;
      expect(full, 'the reader is holding an over-window history').toBeGreaterThan(8);

      // The column now holds everything: re-arming the follow there must not
      // trim rows the reader can see.
      setMeasured(300, 900);
      resized(100);
      await tick();
      expect(last()?.length, 'a fitting column keeps its rows').toBe(full);

      // Overflowing again and back at the foot, the oldest pages go.
      setMeasured(900, 320);
      resized(100);
      list()?.scrolledTo(580, 900, 320);
      await tick();
      expect(last()?.length ?? 0, 'the foot settles on the newest window').toBeLessThan(full);
    });

    /**
     * **A landing under a fitting column drops nothing.** The settle's bound is
     * the row at the column's top edge, which on a column that holds everything
     * is row 0: a settle written as a budget check alone would drop the oldest
     * pages under a reader who can see all of them (#2532 review).
     */
    it('keeps every row when a page lands under a fitting column', async () => {
      const server = stub();
      draw(server);
      setMeasured(900, 320);
      server.page([turn('t0')], 'cursor-0');
      await tick();
      list()?.scrolledTo(0, 900, 320);
      flushSync();
      for (let i = 1; i <= 8; i += 1) {
        server.page([heavy(`h${i}`)], `cursor-${i}`);
        await tick();
      }
      const full = last()?.length ?? 0;
      expect(full, 'the reader is holding an over-window history').toBeGreaterThan(8);

      // Everything fits, and the follow is re-armed there.
      setMeasured(300, 900);
      resized(100);
      await tick();

      // A page lands while the column fits: the top edge rests on row 0, so the
      // settle may drop nothing the reader can see.
      server.page([heavy('h9')], 'cursor-9');
      await tick();
      expect(last()?.length, 'the page landed and no row under the reader went').toBe(full + 1);
    });

    /**
     * **A landing while the reader is at the latest settles the window.** The
     * page arrives while the follow is on and the column scrolls; the column is
     * what settles it, because it is what knows whether the whole conversation
     * is on screen (a fitting column must keep the page it just pulled).
     */
    it('settles the window when a landing arrives while the reader is at the latest', async () => {
      const server = stub();
      draw(server);
      setMeasured(900, 320);
      server.page([turn('t0')], 'cursor-0');
      await tick();
      // The reader is at the foot of a column that scrolls.
      list()?.scrolledTo(580, 900, 320);
      await tick();
      const before = last()?.length ?? 0;

      // One landing of a single row over the window on its own.
      const handed = records.length;
      server.page([heavy('h1', OVER_WINDOW_MIB)], 'cursor-1');
      await tick();
      await tick();

      // **The list was handed the landing before the settle.** The settled
      // count alone also passes had the page been dropped before it ever
      // appended, which is the case this witness separates out.
      expect(
        records.slice(handed).map((row) => row.length),
        'the landing arrived',
      ).toContain(before + 1);
      // **And the row left is the newest**, not the page that landed above it.
      expect(
        [...document.querySelectorAll('.conv [data-t]')].map((row) => row.getAttribute('data-t')),
        'the settle kept the newest row and dropped the page above it',
      ).toEqual(['t0']);
      expect(last()?.length ?? 0, 'the landing settled the window').toBe(before);
    });
  },
);

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

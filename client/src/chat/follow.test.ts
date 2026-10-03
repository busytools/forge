// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { writable } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage } from '../protocol';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';

/**
 * The list is a stub, because what has to be seen is what the column HANDS it.
 *
 * The follow is a decision about the reader's place, and the only thing it
 * does is scroll - so the calls are the whole of the behaviour, and a real
 * list would hide them behind its own measurement. The stub's element reports
 * the size a test gives it and clamps what it is asked for, the way a browser
 * clamps a scroll, so a test can say where a pin LANDS rather than only what
 * it asked for.
 */
vi.mock('virtua/svelte', async () => {
  const { default: List } = await import('./testing/List.svelte');
  return { VList: List };
});

// The column's record is frozen where it is handed out, so a write into any
// part of it - the scroll and resize paths included - throws here too.
vi.mock('./conversation', async (importOriginal) => {
  const { frozenConversation } = await import('./testing/frozen');
  return frozenConversation(await importOriginal<typeof import('./conversation')>());
});

const { clear, element, list, pinned, setElement, setMeasured } = await import('./testing/records');
const { default: Chat } = await import('./Chat.svelte');
const { default: Seats } = await import('./testing/Seats.svelte');

const { clearObservers, installResizeObserver, resized } = await import('./testing/viewport');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };
/** The seat the column is moved to, for the test that changes it under a reader. */
const OTHER: SessionSlot = { org: 'Busytools', project: 'forge', label: 'other' };

/** One turn as a page carries it. */
const turn = (key: string): unknown => ({
  key,
  messages: [{ type: 'user', uuid: `u-${key}`, message: { role: 'user', content: [] } }],
});

/**
 * One turn carrying words, which is what draws a row the anchor can hold.
 *
 * A user message with no content draws nothing at all, so the empty `turn`
 * above has no `data-k` row to find.
 */
const spoken = (key: string): unknown => ({
  key,
  messages: [
    {
      type: 'user',
      uuid: `u-${key}`,
      message: { role: 'user', content: [{ type: 'text', text: `${key} said` }] },
    },
  ],
});

/**
 * Lay the drawn rows out by hand, because jsdom performs no layout.
 *
 * Each row gets a box `height` tall from the column's own top, in the order the
 * document holds them, less the reader's offset - which is what a rect is, and
 * read LIVE off the element, because the offset moves after this is called and
 * a browser's box is whatever it is at the moment it is asked. The first row can
 * be given a different height, which is how one above the reader grows under
 * them.
 */
function layOut(height: number, leader = height): void {
  const root = document.querySelector('.conv');
  if (root === null) return;
  [...root.querySelectorAll('.turn [data-k]')].forEach((row, at) => {
    const tall = at === 0 ? leader : height;
    const top = at * height + (at === 0 ? 0 : leader - height);
    row.getBoundingClientRect = () => ({
      top: top - element.offset,
      bottom: top + tall - element.offset,
      height: tall,
      width: 0,
      left: 0,
      right: 0,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    });
  });
}

/**
 * A console a thousand pixels tall, so a reader can be anywhere in it.
 *
 * **Fractional on purpose.** `virtua` reports its size as a fraction of a
 * pixel and the browser clamps the scroll to a whole one, so a list whose total
 * is a round number is the one case where comparing the two straight happens to
 * work - which is how the follow came to switch itself off at the foot.
 */
const TOTAL = 1000.31;
const VIEWPORT = 300;
/** What the column asks for when it pins the foot, and what that lands on. */
const ASKED = Math.floor(TOTAL);
const FOOT = ASKED - VIEWPORT;

/** The pin a column at the foot makes, which both of its passes are. */
const PIN = { asked: ASKED, landed: FOOT };
/** The pin a column whose whole history fits makes: it asks for the end and the clamp puts it at the start. */
const FITS = { asked: VIEWPORT, landed: 0 };

function stub() {
  const listeners = new Set<(message: ServerMessage) => void>();
  const watchers = new Set<(status: 'closed' | 'open') => void>();
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (_conversation: SessionSlot, _before: string | null, _turns: number) => true,
    onMessage: (fn: (message: ServerMessage) => void) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onStatus: (fn: (status: 'closed' | 'open') => void) => {
      watchers.add(fn);
      return () => watchers.delete(fn);
    },
    store: () => undefined,
    settings: () => null,
    status: () => 'open' as const,
    close: () => undefined,
  } as unknown as Connection;

  const send = (message: ServerMessage): void => {
    for (const fn of listeners) fn(message);
    flushSync();
  };

  return {
    connection,
    /** The socket drops, the way it does mid-ask. */
    drop(): void {
      for (const fn of watchers) fn('closed');
      flushSync();
    },
    /**
     * A page landing. `cursor` says there are older turns above it, which is
     * what lets the column ask for them at all - `null` is a page with nothing
     * behind it, and no ask follows.
     */
    page(turns: unknown[], seat: SessionSlot = LEAD, cursor: string | null = null): void {
      send({ kind: 'page', conversation: seat, turns, cursor });
    },
    /** The seat changes occupant under the column, the way `/new` lands. */
    replaced(): void {
      send({
        kind: 'update',
        update: { session_replaced: { key: LEAD, session_id: 'new-occupant' } },
      });
    },
    /**
     * A refusal of an ask. `seat` is what the server names where the refusal is
     * about a seat; an older server names none.
     */
    refuse(seat?: SessionSlot): void {
      send({
        kind: 'error',
        what: 'more',
        why: 'forge holds no session for this seat',
        ...(seat === undefined ? {} : { seat }),
      });
    },
    /** One frame arriving on the seat, the way a running turn's do. */
    frame(): void {
      send({
        kind: 'update',
        update: { chat_appended: { key: LEAD, msg: said('a line arriving') } },
      });
    },
    /** A prompt the reader sends, as the CLI echoes it back on the seat. */
    prompt(): void {
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: LEAD,
            msg: {
              type: 'user',
              uuid: 'said-once',
              message: { role: 'user', content: [{ type: 'text', text: 'what is this' }] },
            },
          },
        },
      });
    },
  };
}

function said(text: string): unknown {
  return {
    type: 'assistant',
    uuid: `live-${text}`,
    message: { role: 'assistant', content: [{ type: 'text', text }] },
  };
}

/** Let the list mount, the frame's layout run, and every deferred pass land. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  // **Two painted frames, because the fold's draw lands on one.** A frame's
  // update publishes the record on the painted frame after it arrives; the
  // effects that respond run behind that publish; and the passes they defer -
  // the pin's second measure, the observer's restore - are the next paint's.
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  flushSync();
}

let app: Record<string, unknown> | null = null;

/** Where the reader is, which is what the follow is a decision about. */
function readerAt(offset: number): void {
  list()?.scrolledTo(offset, TOTAL, VIEWPORT);
  flushSync();
}

/** A clamp with no scroll event, which is what a resize can do to the reader. */
function clamped(at: number, total = TOTAL, height = VIEWPORT): void {
  list()?.settled(at, total, height);
}

/**
 * A column on a seat with two turns, left where the landing put it.
 *
 * The list only mounts once there is something to draw, so the page has to
 * land before the reader has a place at all - and what the column does with
 * that landing is one of the things under test, so this does not touch it.
 */
async function draw(server: ReturnType<typeof stub>): Promise<void> {
  clear();
  clearObservers();
  // A list has measured its rows before a column asks it anything, so the
  // landing's own pin reads a list of a real height rather than of none.
  setMeasured(TOTAL, VIEWPORT);
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection },
  });
  // The column listens on the effect that mounts it, so the page has to go out
  // after that has run.
  flushSync();
  server.page([turn('t1'), turn('t2')]);
  await settle();
  expect(list(), 'the list is mounted once there are turns to draw').not.toBeNull();
}

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  clear();
  clearObservers();
});

describe('whether the column follows the newest end', () => {
  it('opens at the foot of the newest page, with nobody having scrolled', async () => {
    const server = stub();
    await draw(server);

    // Nothing has touched the list here: these pins are the landing's own, and
    // they are what says the column OPENS following rather than following once
    // the reader has moved.
    expect(pinned(), 'the page landing pins the foot by itself').toEqual([PIN, PIN]);
  });

  it('pins the foot again when a frame lands below the reader', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    server.frame();
    await settle();

    // One pin now and one after this frame's layout: a row that grew in this
    // same update is measured a moment later, and the foot it moves to is what
    // the second pass is for.
    expect(pinned(), 'the frame is what put these here').toEqual([PIN, PIN]);
  });

  it('does not move a reader who has scrolled up', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.frame();
    // **The settle is what makes this bite**: the frame draws a painted frame
    // after it arrives, and an assertion read before that draw passes for any
    // follow behaviour at all - an emptiness assertion is the one shape that
    // reads true from a frame that never landed.
    await settle();

    expect(pinned(), 'nothing that lands below them moves the column').toEqual([]);
  });

  it('follows again once the reader is back at the very end', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // Back at the end: the follow re-arms, and what it does with the next
    // frame is the thing under test rather than the re-arm's own pin.
    readerAt(FOOT);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the end they came back to is followed again').toEqual([PIN, PIN]);
  });

  it('leaves a reader parked a few pixels short of the end alone', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // Four pixels short: near enough to look like the end and not the end.
    readerAt(FOOT - 4);
    server.frame();
    // The draw lands a painted frame after the frame, as above: without the
    // settle this passes on the frame not having landed at all.
    await settle();

    expect(pinned(), 'the very end is the rule, not a threshold near it').toEqual([]);
  });

  it('leaves a reader who just scrolled away alone, for the frame in their own breath', async () => {
    // **The disarm is a decision, and it publishes at once.** The reader's
    // scroll away and the frame land in ONE task, with no paint between them;
    // a follow decision deferred to a painted frame would let the frame's own
    // draw read the side before it - the reader back at the foot they just
    // left, pinned by whatever arrived. This is the shape that keeps
    // `following()` on the at-once side of the split.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    readerAt(FOOT - 120);
    server.frame();
    await settle();

    expect(pinned(), 'the reader who just scrolled away stays where they put themselves').toEqual(
      [],
    );
  });

  it('opens at the foot after the seat changes under a scrolled-up reader', async () => {
    const seat = writable(LEAD);
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Seats, {
      target: document.body,
      props: { seat, connection: server.connection },
    });
    flushSync();
    server.page([turn('t1'), turn('t2')]);
    await settle();

    // The reader scrolls up on the seat they are on, so the column's own last
    // placement is in that seat's offsets.
    readerAt(0);
    await settle();
    clear();

    // The seat changes under the same column, and the page of the new one
    // lands: the follow owes that seat an opening at its own foot.
    seat.set(OTHER);
    flushSync();
    expect(list(), 'the handle goes with the list the column left').toBeNull();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();

    expect(pinned(), 'the new conversation opens at its foot').toEqual([PIN, PIN]);
  });

  it('lands back at the foot when the reader returns to a seat they left scrolled up', async () => {
    // **A place belongs to the visit, not to the seat** (#1673). A seat's
    // conversation is kept, and its follow flag was kept with it - so a seat
    // left scrolled up came back with `following: false`, the follow pass
    // returned early, and the reader landed wherever the old offset fell in
    // the history. Every entry lands at the latest. The terminal deliberately
    // does the other thing - its own per-session `auto_scroll` restores where
    // the reader was - and that divergence is named in the PR.
    const seat = writable(LEAD);
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Seats, {
      target: document.body,
      props: { seat, connection: server.connection },
    });
    flushSync();
    server.page([turn('t1'), turn('t2')]);
    await settle();

    // The reader scrolls up on the seat they are on, then leaves it.
    readerAt(0);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // And comes back: the kept conversation lands at ITS foot, following.
    seat.set(LEAD);
    flushSync();
    await settle();

    expect(list(), 'the kept conversation is drawn again').not.toBeNull();
    expect(pinned(), 'the return lands at the foot, not where it was left').toEqual([PIN, PIN]);

    // **And a seat left AT its foot still re-pins on the way back**, which is
    // the half the seat in the pass's key carries: this conversation's follow
    // is already on, so the re-arm writes the same record and nothing else
    // about it changes - without the seat in the key the pass would not
    // re-run, and the reader would land on whatever offset the seat they are
    // leaving was scrolled to.
    await settle();
    clear();
    readerAt(FOOT);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    await settle();
    readerAt(0);
    await settle();
    clear();

    seat.set(LEAD);
    flushSync();
    await settle();

    expect(pinned(), 'a return to a seat left at its foot lands there too').toEqual([PIN, PIN]);
  });

  it('re-arms at the foot the element clamps to while the model reads long', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // The model keeps estimates for rows it has never drawn, and they read
    // longer than the element is. The reader is at the foot the browser
    // clamps them to, and that is the end.
    list()?.scrolledTo(FOOT, TOTAL + 400, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    // **The count is the passes, and the value is the property.** A frame's
    // draw now lands on a painted frame rather than at the write (#1670), so
    // the layout settles over one more pass and the observer pins once more
    // than the effect's pair. What the test is about is WHICH foot every pin
    // asks for - the element's numbers, never the model's longer ones - so
    // that is what is asserted, and a pin asking for anything else fails
    // whichever pass it came from.
    const pins = pinned();
    expect(pins.length, 'the column pinned, over its passes').toBeGreaterThanOrEqual(2);
    expect(
      [...new Set(pins.map((pin) => JSON.stringify(pin)))],
      "and every pin asked for the element's foot, not the model's",
    ).toEqual([JSON.stringify(PIN)]);
  });

  it('keeps following when its own pin lands before the list has measured', async () => {
    const server = stub();
    await draw(server);
    clear();

    // The pin asked for the foot as it was drawn, and its own scroll event
    // follows in the same flush - before the list has measured its viewport.
    // The reader has not moved; the column's own pin did.
    list()?.scrolledTo(FOOT, TOTAL, 0);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the pin is the column moving the reader, not the reader moving').toEqual([
      PIN,
      PIN,
    ]);
  });

  it('keeps following when the rows settle taller than its own pin', async () => {
    const server = stub();
    await draw(server);
    clear();

    // Both the element and the model settle past the height the pin asked
    // for: the pin's scroll event arrives with the reader above the foot it
    // aimed at, having moved nobody.
    setElement(TOTAL + 400, VIEWPORT);
    list()?.scrolledTo(FOOT, TOTAL + 400, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the pin left the reader at the foot it could see').toEqual([
      { asked: ASKED + 400, landed: FOOT + 400 },
      { asked: ASKED + 400, landed: FOOT + 400 },
    ]);
  });

  it('keeps following when a read lands between the pin and its own echo', async () => {
    // **Measured on the switch into the giant seat (2026-10-03): the follow
    // turned itself off mid-open and left the reader 175,859px above the
    // foot.** The column's own effect was re-running while the seat was
    // unchanged - the live page ran it again with the seat, the slot and the
    // connection all identical - and each re-run cleared the placement the
    // landing's pin had just recorded. The pin's echo then read as a reader
    // who had moved, and the follow switched itself off mid-landing.
    //
    // This drives one shape of that re-run: the same three parts, handed
    // over as a fresh object.
    const seat = writable(LEAD);
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Seats, {
      target: document.body,
      props: { seat, connection: server.connection },
    });
    flushSync();
    server.page([turn('t1'), turn('t2')]);
    await settle();
    clear();

    // The same seat, handed over again.
    seat.set({ ...LEAD });
    flushSync();
    await settle();

    // The pin's own echo, with the content that arrived before it dispatched:
    // the pin landed on the total as the list had it, and the reader is above
    // the foot that newer total has - having moved nobody.
    setElement(TOTAL + 400, VIEWPORT);
    list()?.scrolledTo(FOOT, TOTAL + 400, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the landing is the column moving the reader, not the reader moving').toEqual([
      { asked: ASKED + 400, landed: FOOT + 400 },
      { asked: ASKED + 400, landed: FOOT + 400 },
    ]);
  });

  /**
   * **The place a parked reader holds is put back when the layout moves under
   * them.** The boxes are synthetic, since jsdom performs no layout, but the
   * relation is the whole of it: a row above the reader grows by 200 and the
   * column's own scroll follows by 200, holding the row where their eye was.
   *
   * It is also the wiring's arm: with the restore's write disabled this case is
   * the only one in the suite that notices - the math in `anchor.test.ts` pins
   * the relation, but nothing else asks the column to use it.
   *
   * **And this and the three restores beside it are where the capture's revert
   * dies** (measured): restoring the old `if (!held.following)` read AND
   * deferring the follow write turns exactly these four red, because the
   * capture then reads the side before the write and never happens. Under the
   * shipped split the re-read happens to be fresh, so a revert is silent by
   * construction - the fix is fragility removed, and these four are what
   * catches it if the write's timing moves again.
   */
  it('puts a parked reader back on their row when the layout moves above them', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // No cursor: nothing above this page, so the reader's scroll asks for
    // nothing and the compensation stays off - which is what makes this arm
    // about the restore alone.
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    expect(list(), 'rows to hold').not.toBeNull();

    // Rows 40px tall, the reader 50px down: inside the second row, 10px into it.
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The first row grows by 200, so the reader's row starts 200 further down.
    layOut(40, 240);
    server.frame();
    await settle();

    expect(element.offset, "the reader's row carried them down with it").toBe(250);
  });

  /**
   * **A size change with an ask in flight is the compensation's business.**
   * Near the top a page of older turns is asked for, and while that ask is out
   * the restore stands aside - the list is holding the reader by its own shift,
   * and a row above them growing is not theirs to compensate. (Before the
   * guard this path restored anyway: the offset would read 250 here.)
   */
  it('leaves a prepend in flight to the list, not to the restore', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')], LEAD, 'c1');
    await settle();

    // 40px rows, the reader 50 down - inside the second row, near enough to the
    // top that the column asks for the turns above.
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The first row grows by 200 and the browser reports a size change: the
    // observer's own path, while the ask is out.
    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, 'the compensation is still on').toBe(50);
  });

  /**
   * **And when the ask is gone, the restores come back.** A dropped socket
   * takes a page in flight with it and the reconnect asks again - so the
   * conversation says it forgot the ask, and the column's own count drains.
   * Without that drain the count outlives the ask, `shift` stays on for the
   * life of the seat, and this PR's own hold quietly stops working: the offset
   * left at 50 where the row above the reader had moved it to 250.
   */
  it('drains a dropped ask, and the row holds the reader again', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')], LEAD, 'c1');
    await settle();

    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The socket drops with that ask in flight, and the reader stays put.
    server.drop();
    await settle();

    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, 'the row carried them down once the ask was drained').toBe(250);
  });

  /**
   * **An occupant swap forgets an ask too, and has to say so.** The swap resets
   * the conversation (a page for it can name no occupant), and that reset
   * zeroes the count the column drains against - so without the swap reporting
   * its forgotten ask, the column keeps holding one, `shift` stays armed for
   * the life of the seat, and the observer's restore never runs for a parked
   * reader (measured: the offset left at 50 where the row above them had moved
   * it to 290).
   */
  it('reports the ask an occupant swap forgot, so the drain fires', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')], LEAD, 'c1');
    await settle();

    // Parked near the top with an ask in flight, then the seat changes
    // occupant under them and the new one's page lands.
    layOut(40);
    readerAt(50);
    await settle();
    server.replaced();
    await settle();
    // **Two pages, because a swap is owed one it must drop**: a page carries
    // neither an id nor an occupant, so the count of asks told to forget is
    // what tells the old occupant's late answer from the new one's, and the
    // first page after a swap is spent on it. No cursor on the new occupant's
    // page: nothing above it, so the re-park below asks for nothing and only
    // the swap's forgotten ask is in play.
    server.page([spoken('stale1'), spoken('stale2')]);
    await settle();
    server.page([spoken('o1'), spoken('o2'), spoken('o3')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    layOut(40, 280);
    resized();
    await settle();

    expect(element.offset, 'the row carried them down once the swap was drained').toBe(290);
  });

  /**
   * **And a refusal for another seat is not this seat's.** Every chat on the
   * shared connection hears every error, and a background seat's refusal is the
   * everyday no-session-yet state, re-asked every couple of seconds - draining
   * this seat's count on it would let the restore run in the middle of a
   * prepend it must leave alone. Both directions: the other seat's refusal
   * leaves this count armed, and this seat's own drains it.
   */
  it("keeps another seat's refusal out of this seat's count", async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')], LEAD, 'c1');
    await settle();

    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // A background seat's refusal, then a row above the reader grows.
    server.refuse(OTHER);
    await settle();
    layOut(40, 280);
    resized();
    await settle();
    expect(element.offset, 'the ask this seat is holding is still armed').toBe(50);

    // This seat's own refusal drains it, and the observer's path restores.
    server.refuse(LEAD);
    await settle();
    resized();
    await settle();
    expect(element.offset, 'the row carried them down once their own refusal landed').toBe(290);
  });

  it('brings the reader back for their own prompt', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.prompt();
    await settle();

    expect(pinned(), 'a prompt is where the reader wants to be, wherever they were').toEqual([
      PIN,
      PIN,
    ]);
  });

  it('re-pins when the content changes size under a reader at the foot', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    // A row laid out late, a disclosure opened, a code block measured: the
    // browser reports the size change, and the foot moved with it.
    resized();

    expect(pinned(), 'a size change moves the foot, so it re-pins it').toEqual([PIN]);
  });

  it('leaves a scrolled-up reader alone when the content changes size', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    resized();

    expect(
      pinned(),
      'a reader who scrolled away is not yanked to the end by anything arriving',
    ).toEqual([]);
  });

  it('leaves a scrolled-up reader alone when the column grows under them', async () => {
    // The column's own height is not only the conversation's: the composer
    // below it changes shape - a dictation row appearing, a panel closing, the
    // box getting taller - and the reader's offset does not move with it. A
    // reader a little way up then measures as being at the very end, because
    // they are at the end of what now fits. The arithmetic is right and the
    // conclusion is wrong: they did not move, and the end is not where they
    // put themselves.
    const server = stub();
    await draw(server);
    readerAt(620);
    await settle();
    // The landing's own pin has been and gone, and the reader's scroll up has
    // left nothing following.
    clear();
    expect(pinned(), 'the reader is scrolled up, so nothing is pinned').toEqual([]);

    setElement(TOTAL, 400);
    resized();
    await settle();
    clear();

    server.frame();
    await settle();

    expect(
      pinned(),
      'following is the reader own decision, and only their own moving changes it',
    ).toEqual([]);
  });

  it('leaves a scrolled-up reader alone when a clamp fires the scroll event', async () => {
    // **The same size change, through the other channel.** A row corrected to
    // its drawn height takes height out of the column, and the browser clamps
    // the reader down with the content AND fires a scroll event landing at the
    // foot - so a resize that no longer re-arms through the observer can still
    // re-arm here, and the reader is carried back by whatever arrived.
    const server = stub();
    await draw(server);
    readerAt(620);
    await settle();
    clear();

    const shorter = TOTAL - 200;
    setElement(shorter, VIEWPORT);
    list()?.scrolledTo(shorter - VIEWPORT, shorter, VIEWPORT);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the event a clamp fires is not a reader arriving at the end').toEqual([]);
  });

  it('keeps following when a clamp moves the reader with the content', async () => {
    // The mirror of the case above: a row corrected SHORTER leaves a following
    // reader above the foot, and a scroll event that reads as them scrolling
    // away is a column that stops following mid-stream and stays stopped.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    const shorter = TOTAL - 200;
    setElement(shorter, VIEWPORT);
    list()?.scrolledTo(shorter - VIEWPORT, shorter, VIEWPORT);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the column is still following, and pins the foot again').not.toEqual([]);
  });

  it('re-arms when a size change leaves the reader at the very end', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // A window grown until the history fits: the whole of it is on screen at
    // once, so the browser leaves the reader at the end of it and fires no
    // scroll event to say so. Nothing is left to scroll, so being at the end is
    // not a position anybody chose - which is the one size change that may put
    // the follow back on.
    setElement(VIEWPORT, VIEWPORT);
    clamped(0, VIEWPORT, VIEWPORT);
    resized();
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the end they are sitting at is followed again').toEqual([FITS, FITS]);
  });
});

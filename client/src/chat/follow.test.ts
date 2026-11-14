// @vitest-environment jsdom
import { readFileSync } from 'node:fs';

import { flushSync, mount, unmount } from 'svelte';
import { get, writable } from 'svelte/store';
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

// The perf surface, observed: the give-up's only record outside the column is
// its scroll line, and the marker it writes is what the desktop log reads.
vi.mock('../perf', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../perf')>();
  return { ...actual, noteScrollMove: vi.fn() };
});

const {
  clear,
  element,
  layout,
  list,
  measured,
  moved,
  pinned,
  quantised,
  reported,
  setElement,
  setMeasured,
  setModel,
} = await import('./testing/records');
const { conversationsFor, places, releaseConversation } = await import('./conversations');
const { echoes } = await import('./echoes.svelte');
const { noteScrollMove } = await import('../perf');
const { subjectKey } = await import('../protocol');
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
 * One frame as the live path first had it: no id, so `fold` keys its row by
 * its position in the turn.
 */
const idless = (role: 'user' | 'assistant', text: string): unknown => ({
  type: role,
  message: { role, content: [{ type: 'text', text }] },
});

/**
 * One turn carrying a message big enough to put the held window over its
 * 8 MiB budget, which is the only way the client's own trim drops a page.
 */
const huge = (key: string, bytes: number): unknown => ({
  key,
  messages: [
    {
      type: 'user',
      uuid: `u-${key}`,
      message: { role: 'user', content: [{ type: 'text', text: 'x'.repeat(bytes) }] },
    },
  ],
});

/** The same frame as the read carries it, with the id the CLI minted. */
const identified = (uuid: string, role: 'user' | 'assistant', text: string): unknown => ({
  type: role,
  uuid,
  message: { role, content: [{ type: 'text', text }] },
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
  const live = (): Element[] => [...root.querySelectorAll('.turn [data-k]')];
  live().forEach((row) => {
    // The place is read at ASK time, not at assignment time: a row that
    // reorders with its key keeps its element (the list is keyed), so a
    // position captured here would go stale the moment the order moved.
    row.getBoundingClientRect = () => {
      // **Counted, because a box read is what a parked pass COSTS** - the
      // same measurement the perf rig takes (its gbcr counter) - and the
      // "not on every frame" half of #1734's acceptance is pinned on it.
      rectReads += 1;
      const at = live().indexOf(row);
      const tall = at === 0 ? leader : height;
      const top = at * height + (at === 0 ? 0 : leader - height);
      return {
        top: top - element.offset,
        bottom: top + tall - element.offset,
        height: tall,
        width: 0,
        left: 0,
        right: 0,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      };
    };
  });
}

/** How many row boxes the column has read, reset with the suite's records. */
let rectReads = 0;

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
  /** Every ask for older turns the column made, by the cursor it asked from. */
  const asks: Array<string | null> = [];
  /** How many results this seat has sent, so each settles its own turn. */
  let results = 0;
  const connection = {
    subscribe: () => ({ state: () => ({ kind: 'ready' as const }) }),
    unsubscribe: () => undefined,
    refresh: () => undefined,
    dispatch: () => null,
    more: (_conversation: SessionSlot, before: string | null, _turns: number) => {
      asks.push(before);
      return true;
    },
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
  /** One frame arriving on the seat, the way a running turn's do. */
  const frameMessage = (): ServerMessage => ({
    kind: 'update',
    update: { chat_appended: { key: LEAD, msg: said('a line arriving') } },
  });
  /** The reader's own prompt, as the dispatch forges it on the seat. */
  const promptMessage = (): ServerMessage => ({
    kind: 'update',
    update: {
      chat_appended: {
        key: LEAD,
        source: 'you',
        msg: {
          type: 'user',
          uuid: 'said-once',
          message: { role: 'user', content: [{ type: 'text', text: 'what is this' }] },
        },
      },
    },
  });

  /**
   * The press this client makes before its command goes.
   *
   * The composer posts the echo with the prompt id it minted, and the
   * conversation reads a frame's id against it to tell this client's own send
   * from the reader's send on another client. A test that models a local send
   * has to make that press, or the frame is a stranger's.
   */
  const localPress = (running: boolean): void => {
    echoes.post(subjectKey({ session: LEAD }), 'what is this', running, 'said-once', echoes.mint());
  };

  return {
    connection,
    /** Every ask for older turns the column made, by the cursor it asked from. */
    asks,
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
    replaced(to = 'new-occupant'): void {
      send({
        kind: 'update',
        update: { session_replaced: { key: LEAD, session_id: to } },
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
      send(frameMessage());
    },
    /** The turn's own result, which ends the row a prompt opened. */
    result(): void {
      results += 1;
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: LEAD,
            msg: { type: 'result', uuid: `result-${results}` },
          },
        },
      });
    },
    /**
     * A prompt the reader sends, as the dispatch forges it on the seat.
     *
     * `source` rides the forged frame and says whose words it carries - the
     * same vocabulary the queued row uses - so the two frames that announce
     * one prompt agree about its sender.
     */
    prompt(): void {
      localPress(false);
      send(promptMessage());
    },
    /**
     * The reader's send and the turn's next frame in ONE task, so the fold
     * takes both before any pass runs between them.
     */
    promptThenFrame(): void {
      localPress(false);
      for (const fn of listeners) {
        fn(promptMessage());
        fn(frameMessage());
      }
      flushSync();
    },
    /**
     * A frame this client's own press put on the wire, by the id it minted.
     *
     * No echo is posted: the surfaces that answer a takeover or send from the
     * palette carry no row of their own, and the id is what says the words are
     * this client's.
     */
    ownFrame(uuid: string, text: string, seat: SessionSlot = LEAD): void {
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: seat,
            source: 'you',
            msg: {
              type: 'user',
              uuid,
              message: { role: 'user', content: [{ type: 'text', text }] },
            },
          },
        },
      });
    },
    /** A prose turn on another seat, as the one connection carries the fleet. */
    forgedOn(seat: SessionSlot, source = 'cron'): void {
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: seat,
            source,
            msg: {
              type: 'user',
              uuid: `forged-${source}-${seat.label}`,
              message: { role: 'user', content: [{ type: 'text', text: 'keep going' }] },
            },
          },
        },
      });
    },
    /**
     * A prose turn the core forged for words that are nobody's send: forge's
     * own machinery writing as a user row - an auto-continue, a worker kick,
     * a board notice. It rides the frame a person's own send does, and the
     * source is the one thing that tells the two apart.
     */
    forged(source = 'forge'): void {
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: LEAD,
            source,
            msg: {
              type: 'user',
              uuid: `forged-${source}`,
              message: { role: 'user', content: [{ type: 'text', text: 'keep going' }] },
            },
          },
        },
      });
    },
    /**
     * A repaired stretch, in answer to a range ask: the frames the seat
     * emitted while the reader was away, numbered as they were.
     */
    repaired(...texts: string[]): void {
      send({
        kind: 'seat_frames',
        slot: LEAD,
        epoch: null,
        frames: texts.map((text, at) => ({
          seq: at + 1,
          update: {
            chat_appended: {
              key: LEAD,
              source: 'you',
              msg: {
                type: 'user',
                uuid: `repaired-${text}`,
                message: { role: 'user', content: [{ type: 'text', text }] },
              },
            },
          },
        })),
      });
    },
    /** The CLI took the queued prompt: the drain draws its held row. */
    started(): void {
      send({
        kind: 'update',
        update: { prompt_lifecycle: { key: LEAD, uuid: 'said-once', state: 'started' } },
      });
    },
    /** The same lifecycle for a prompt the read's queue named under another id. */
    startedFor(uuid: string): void {
      send({
        kind: 'update',
        update: { prompt_lifecycle: { key: LEAD, uuid, state: 'started' } },
      });
    },
    /** The seat's own record naming its occupant, the way a subscribe does. */
    session(id: string): void {
      send({
        kind: 'snapshot',
        subject: { session: LEAD },
        data: { header: { session_id: id } },
      });
    },
    /** The core's answer on whether a turn is in flight, which a page alone cannot say. */
    inflight(on = true): void {
      send({
        kind: 'snapshot',
        subject: { session: LEAD },
        data: { header: { turn_in_flight: on } },
      });
    },
    /**
     * The read's own queue, which the snapshot carries.
     *
     * **For a client that attaches mid-queue this is the only word it ever
     * gets about those prompts**: the pending backlog of `prompt_queued`
     * frames goes to the first-ever subscriber alone, so the sender of these
     * words is knowable here and nowhere else.
     */
    readQueue(rows: Array<{ uuid: string; source: string; text: string }>): void {
      send({
        kind: 'snapshot',
        subject: { session: LEAD },
        data: { header: { turn_in_flight: true }, state: { queue: rows } },
      });
    },
    /**
     * The core saying that prompt is waiting in the CLI's queue, which retracts
     * its row into the hold. `source` rides every queued row on the wire and
     * says who sent it: the reader's own send is `you`, and a cron fire, a
     * peer message, a delivery or forge's own continuation each carry its
     * own.
     */
    queued(source = 'you'): void {
      // The reader's own mid-turn press is this client's, so the echo goes
      // first; a queue announcement from anyone else carries another id.
      if (source === 'you') localPress(true);
      send({
        kind: 'update',
        update: { prompt_queued: { key: LEAD, uuid: 'said-once', source, text: 'what is this' } },
      });
    },
    /**
     * A prompt the reader sent from ANOTHER client: the wire says `you`, and
     * the id is one this client never minted.
     */
    remotePrompt(): void {
      send({
        kind: 'update',
        update: {
          chat_appended: {
            key: LEAD,
            source: 'you',
            msg: {
              type: 'user',
              uuid: 'from-the-phone',
              message: { role: 'user', content: [{ type: 'text', text: 'sent from the phone' }] },
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

/**
 * One animation frame, exactly.
 *
 * `drained` waits for the landing's passes to stop by condition; a test about
 * what a frame SAW - the capture's two-frame agreement, the give-up's budget
 * - needs each frame's layout to be the one the test set, so it pumps one at
 * a time.
 */
async function frame(): Promise<void> {
  await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
  flushSync();
}

/**
 * Wait until the landing's passes have stopped landing.
 *
 * **`settle`'s pumps are a race with the machine.** The page's publish, the
 * pin it lands and the pass that re-measures it each take a paint, and a slow
 * first transform - a cold cache, a loaded box - can push the last of them
 * past the fixed pumps, where it lands after a test's `clear()` and reads as
 * the change under test: a spurious foot pin (#2174's flake, one file over).
 * So the landing is drained by CONDITION instead: wait for its pin, then pump
 * until no further pass lands.
 */
async function drained(): Promise<void> {
  for (let i = 0; i < 100 && pinned().length === 0; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    flushSync();
  }
  let stable = 0;
  let seen = -1;
  for (let i = 0; i < 10 && stable < 2; i += 1) {
    const now = pinned().length;
    stable = now === seen ? stable + 1 : 0;
    seen = now;
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    flushSync();
  }
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
  // The landing is drained before any test measures: a pass still on its way
  // would otherwise land inside the window a test is reading.
  await drained();
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
  rectReads = 0;
  // **The saved places outlive the connection**, so they outlive a test too.
  // Every case starts from no place of its own: a place is a fact about a
  // conversation, and two tests are two app runs.
  for (const which of [LEAD, OTHER]) {
    const seat = subjectKey({ session: which });
    places.forget(seat);
    echoes.clear(seat);
  }
  // **The suite runs as a reader who asked for less motion.** The glide has
  // its own cases, which turn the query off; every other case is about where
  // the column lands and reads the one write that landing is.
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches: query === '(prefers-reduced-motion: reduce)',
  }));
});

describe('whether the column follows the newest end', () => {
  it('opens at the foot of the newest page, with nobody having scrolled', async () => {
    const server = stub();
    await draw(server);

    // Nothing has touched the list here: this pin is the landing's own, and it
    // is what says the column OPENS following rather than following once the
    // reader has moved. One pass, because this frame's layout did not move the
    // height: a second look finding the same foot is a write and an event per
    // frame, which is the cost issue #1710 measured.
    expect(pinned(), 'the page landing pins the foot by itself').toEqual([PIN]);
  });

  it('pins the foot again when a frame lands below the reader', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    server.frame();
    await settle();

    // One pass, because this frame carried no layout change: the second look
    // is for a row laid out after this update's effects ran, and its own test
    // below moves the height between the two passes to see it land.
    expect(pinned(), 'the frame is what put these here').toEqual([PIN]);
  });

  it('re-pins the foot when the observer reports a late layout', async () => {
    // **The observer owns the post-layout pass** (#1710, #1734): a row laid
    // out after this update's effects ran moves the foot a moment later, and
    // a size change is the one way the layout reports itself - so the pass is
    // the observer's, fired here as a browser fires it, rather than an rAF
    // scheduled on every frame that arrived.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    server.frame();
    await settle();
    // The late layout, after the frame's own pin has landed: the row reported
    // its height, so the list's measurement and the element move together.
    setModel(TOTAL + 80);
    element.height = TOTAL + 80;
    resized();
    await settle();

    const grown = Math.floor(TOTAL + 80);
    expect(pinned(), 'the late layout lands one more pin, at the new foot').toEqual([
      PIN,
      { asked: grown, landed: grown - VIEWPORT },
    ]);
  });

  /**
   * A layout move that carries a following column toward the foot, short of
   * it: the content grows and the offset moves down with the rows above.
   * `shortBy` is how far past the move the new foot sits.
   */
  function layoutMovesShort(shortBy = 300): number {
    const grown = Math.floor(TOTAL + 400);
    setElement(grown, VIEWPORT);
    list()?.scrolledTo(grown - VIEWPORT - shortBy, grown, VIEWPORT);
    flushSync();
    return grown;
  }

  it('keeps the foot when a layout move carries the reader short of it, with no hand', async () => {
    // **The browser's own scroll anchoring, at an entry.** Rows above measure
    // while the page settles, the offset moves down with them, and the foot
    // moves further still - nobody touched the column, so the hold must not
    // die here and leave every later growth behind it.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    const grown = layoutMovesShort();

    // **The move's own pass is the one under test.** The observer's pass that
    // follows it also lands the foot, so a branch that only re-armed the hold
    // would read green if this read waited for the resize.
    expect(pinned(), 'the layout move itself lands the foot').toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);

    clear();
    resized();
    await settle();

    expect(pinned(), 'and the observer keeps it there').toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
  });

  it('does not re-land a reader whose own touch moved them short', async () => {
    // The move is the layout's, but the hand is theirs: a touch aims at
    // nothing and arms the whole gesture, so a move it could have made is the
    // reader's, not one to correct.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new Event('touchstart'));
    layoutMovesShort();

    expect(pinned(), 'their own touch made the move').toEqual([]);
  });

  it('does not re-land a down-wheel that lands short of the foot', async () => {
    // A wheel down aims at the foot without touching the reader's arm, so the
    // aimed token is the only thing that can say the move was theirs.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: 120 }));
    layoutMovesShort();

    expect(pinned(), 'their own wheel made the move').toEqual([]);
  });

  it('leaves a reader inside the notch alone, where a re-land would move them', async () => {
    // **The foot's own nearness.** A landing inside `FOOT_NEAR` counts as
    // standing at the foot, so there is nothing for the re-land to correct -
    // and without its own check the branch would write the foot for a move
    // the column already treats as the end.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    layoutMovesShort(24);

    expect(pinned(), 'inside the notch is the foot').toEqual([]);
  });

  it("holds the pin at the list's own end while the browser counts a row it has not measured", async () => {
    // **The blink while a turn is live.** A row that grows is in the browser's
    // scrollHeight at once, and the list measures it a callback later. A scroll
    // written past the measured end is read by the list as a position beyond
    // its content, and it answers by dropping the rows on screen until the
    // measurement lands - the whole conversation flickering frame after frame.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    // The row grew in the DOM; the list has not measured it yet.
    setElement(TOTAL + 40, VIEWPORT);
    server.frame();
    await settle();

    // **The pin stops at the measured end, exactly.** The model's foot is
    // `TOTAL - VIEWPORT` plus the viewport's top padding, which jsdom reports
    // as none today, so the exact number is also what pins that premise.
    expect(pinned().length, 'the lagging pin still lands').toBeGreaterThan(0);
    for (const pin of pinned()) {
      expect(pin.asked, 'the pin holds at the end the list measured').toBe(TOTAL - VIEWPORT);
    }

    // **The budget bounds the retries, not the hold.** Frames past it, with
    // the measurement still behind, still ask the model's end and never the
    // browser's - the fallback a give-up write would be.
    for (let at = 0; at < 3; at += 1) {
      await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
      flushSync();
    }
    for (const pin of pinned()) {
      expect(pin.asked, 'the exhausted budget still holds at the measured end').toBe(
        TOTAL - VIEWPORT,
      );
    }
  });

  it('lands the foot again when the list measures what the browser already counted', async () => {
    // **No size report follows the growth.** The row's own measurement is the
    // list's, not the column's, so the deferred pin is the only thing that can
    // land the reader on the real foot - which is what makes this the retry's
    // own test.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    setElement(TOTAL + 40, VIEWPORT);
    server.frame();
    await settle();

    const before = pinned().length;
    setModel(TOTAL + 40);
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    flushSync();

    const grown = Math.floor(TOTAL + 40);
    expect(pinned().slice(before), 'the deferred pin lands on the measured end').toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
  });

  it('keeps following when a fractional pin reads back below its target', async () => {
    // **The browser rounds the offset it lands on**, so a pin that holds at
    // the list's fractional model foot can read back a half pixel short of
    // it. Latching that as "the reader is not at the foot" stops the follow
    // for good while the stream keeps growing - the opposite of the fix.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();
    quantised(0.5);

    // The browser counts a row the list has not measured, and the model foot
    // is 700.2, which lands on the half-pixel grid at 700.
    setModel(1000.2);
    setElement(TOTAL + 400, VIEWPORT);
    server.frame();
    await settle();
    // `clear` resets the stub's grid, so the second half opts in again.
    clear();
    quantised(0.5);

    server.frame();
    await settle();

    // One pin, for the model foot: a stray repin or an empty list would not
    // say the follow survived.
    const landed = pinned();
    expect(landed, 'the frame after a short read-back writes one pin').toHaveLength(1);
    expect(landed[0]?.asked, 'and it asks for the model foot').toBeCloseTo(700.2, 5);
    expect(landed[0]?.landed, 'which the browser rounds onto the grid').toBe(FOOT);
  });

  it('does not pin onto a slot the list has reserved for the newest turn but not drawn', async () => {
    // **The foot a send lands on.** An appended turn is reserved a slot at the
    // list's estimate and drawn a frame later, so for that frame the element's
    // foot runs past the content: pinning to it lands the reader on empty
    // space, and the clamp back - the row measures shorter than the estimate -
    // sweeps the window down and up again, dropping and re-creating every row
    // in the band. The pass waits for the draw; the size change the
    // measurement lands re-fires it.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    // The list's own boxes: two turns drawn, and the arriving third reserved
    // at 300 with 60 of it drawn - the element's foot counts the reservation.
    const reserved = [
      { top: 0, height: TOTAL / 2 },
      { top: TOTAL / 2, height: TOTAL / 2 },
    ];
    measured(TOTAL + 300, [...reserved, { top: TOTAL, height: 60 }]);
    element.height = TOTAL + 300;

    server.prompt();
    await settle();

    expect(pinned(), 'the reserved slot is not a foot to pin onto').toEqual([]);

    // The measurement lands and the slot corrects to what the row drew.
    setModel(TOTAL + 60);
    measured(TOTAL + 60, [...reserved, { top: TOTAL, height: 60 }]);
    element.height = TOTAL + 60;
    resized();
    await settle();

    const drawn = Math.floor(TOTAL + 60);
    expect(pinned(), 'the corrected foot is pinned once the row is drawn').toEqual([
      { asked: drawn, landed: drawn - VIEWPORT },
    ]);
  });

  it("does not pin onto a queued prompt's held row either, which draws nothing at all", async () => {
    // The queue hold is the same reservation with nothing in it: the prompt's
    // row is retracted into the pile, so the turn stands empty at the list's
    // estimate - a row of zero drawn under a slot of a few hundred. A send
    // mid-turn is exactly this, and its shake is the same sweep.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    const reserved = [
      { top: 0, height: TOTAL / 2 },
      { top: TOTAL / 2, height: TOTAL / 2 },
    ];
    measured(TOTAL + 300, [...reserved, { top: TOTAL, height: 0 }]);
    element.height = TOTAL + 300;

    server.prompt();
    server.queued();
    await settle();

    expect(pinned(), 'a held row is not a foot to pin onto').toEqual([]);

    // The row measures as the nothing it draws, and the foot corrects to it.
    measured(TOTAL, [...reserved, { top: TOTAL, height: 0 }]);
    element.height = TOTAL;
    resized();
    await settle();

    const foot = Math.floor(TOTAL);
    expect(pinned(), 'the corrected foot is pinned once the hold is measured').toEqual([
      { asked: foot, landed: foot - VIEWPORT },
    ]);
  });

  it('lands the reader who takes the way back while a row is still reserved', async () => {
    // **The click is the arm, and the reservation can outlast it.** The
    // way-back control unmounts the moment the follow is on, and a reserved
    // row holds the pin back before it writes - so a click that does not
    // leave the column armed makes the next pass read a reader who never
    // reached the foot, refuse, and strand them with the control gone.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // A third turn opens without arming the follow, its slot reserved and not
    // drawn - the row a send lands just before it draws.
    server.forged('forge');
    await settle();
    const reserved = [
      { top: 0, height: TOTAL / 2 },
      { top: TOTAL / 2, height: TOTAL / 2 },
    ];
    clear();
    measured(TOTAL + 300, [...reserved, { top: TOTAL, height: 60 }]);
    element.height = TOTAL + 300;

    const back = document.querySelector('button.follow');
    expect(back, 'precondition: the way back is drawn').not.toBeNull();
    back?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await settle();
    expect(pinned(), 'the reserved row holds the pin back').toEqual([]);

    // The row draws, and the pass that can write lands the reader the click
    // asked for.
    setModel(TOTAL + 60);
    measured(TOTAL + 60, [...reserved, { top: TOTAL, height: 60 }]);
    element.height = TOTAL + 60;
    resized();
    await settle();

    const drawn = Math.floor(TOTAL + 60);
    expect(pinned(), 'the click lands once the row it waited for is drawn').toEqual([
      { asked: drawn, landed: drawn - VIEWPORT },
    ]);
    expect(element.offset, 'and the reader is at the foot').toBe(drawn - VIEWPORT);
  });

  /**
   * **A repaired stretch fills in underneath, and moves nobody.**
   *
   * A gap the client read in the seat's sequence is repaired silently: the
   * frames fold through the same path a live one does - into their turns -
   * while the reader stays where they were standing. A repair that pinned the
   * foot would drag a reader who scrolled up away from the row they were
   * reading, for frames written before they left.
   */
  it('folds a repaired stretch in without moving a parked reader', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();
    const anchored = element.offset;

    server.repaired('said before the gap', 'and answered, too');
    await settle();

    expect(document.body.textContent ?? '', 'the repaired stretch draws its rows').toContain(
      'said before the gap',
    );
    expect(
      element.offset,
      '**and the anchor holds**: the repair lands under the reader rather than moving them',
    ).toBe(anchored);
    expect(pinned(), 'and the follow is not armed by a repair').toEqual([]);
  });

  /**
   * **A repaired frame the READER wrote is not a send.** The source on the
   * frame says whose words it carries, and a live frame with `you` arms the
   * follow - but a repaired one was written before the reader left, so the
   * arm would carry a parked reader to the foot for their own old words.
   */
  it('does not arm the follow on a repaired frame the reader wrote', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.repaired('what is this');
    await settle();

    expect(pinned(), 'a repair is not news, whatever it carries').toEqual([]);
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

  it('does not move a parked reader for a follow an arrival armed', async () => {
    // **The pin's own rule, apart from the arm that broke it.** The column
    // cannot depend on every arm being right: a follow turned on while the
    // reader is parked is not theirs to travel on unless their own send armed
    // it. This is the change that catches a NEW arrival path arming the
    // follow - the class this fix is about.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();
    const parked = element.offset;

    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'a follow nobody at the foot armed may not move the reader').toEqual([]);
    expect(element.offset, 'the reader stays where they put themselves').toBe(parked);
  });

  it('draws the way back while the follow is on but the foot is out of reach', async () => {
    // **A follow that is not pinning is not a reader at the end.** The flag
    // can be on while the column holds position - the state the gate above
    // builds - and the control is the only way back, so it follows the pin's
    // condition rather than the flag alone.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();

    expect(
      document.querySelector('button.follow'),
      'the way back is drawn while nothing is pinning',
    ).not.toBeNull();
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

    expect(pinned(), 'the end they came back to is followed again').toEqual([PIN]);
  });

  it('does not arm the token for a write that moved nobody, so the clamp is read', async () => {
    // **The exact shape #1710's re-read measured.** A write that lands where
    // the element already is fires no event of its own - and a token armed
    // for it sits there to be eaten by the clamp event the browser fires a
    // moment later. The height bookkeeping then keeps the pre-clamp height,
    // the reader's next scroll reads as a clamp, and the frame after yanks
    // them back to the foot.
    const server = stub();
    await draw(server);
    await settle();
    // The grow, with the reader riding it to the new foot, so the bookkeeping
    // holds the taller height - which is what makes the stale read visible.
    setElement(TOTAL + 400, VIEWPORT);
    resized();
    await settle();
    readerAt(FOOT + 400);
    await settle();
    clear();

    // The correction: the browser clamps the reader to the foot the content
    // has now (no event yet), the observer's land writes the value they
    // already hold - no move, so nothing of its own comes back - and THEN
    // the clamp's own event arrives.
    setElement(TOTAL, VIEWPORT);
    clamped(FOOT);
    resized();
    list()?.scrolledTo(FOOT, TOTAL, VIEWPORT);
    flushSync();

    // The reader leaves the foot: on a correct read of the clamp they are the
    // one moving, and the follow lets them go.
    readerAt(FOOT - 100);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the token ate the clamp, and the follow yanked them back').toEqual([]);
  });

  it("keeps the reach ask the landing's own echo carried", async () => {
    // **A consumed echo is still an event the ask rides**, and this is the
    // silent loss the delta restored: a history shorter than a screen and a
    // half, whose landing PIN moves the reader (0 to the clamped 200) so the
    // stub delivers an echo at all - and 200 is inside REACH, which is where
    // the base app asked for the turns above from.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(500, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // A cursor: there IS something above this page to ask for.
    server.page([turn('t1')], LEAD, 'older');
    await settle();

    // The mount's own first load asks with no cursor; the ask under test is
    // the one from the page's own cursor, and it is the echo's.
    expect(
      server.asks.filter((before) => before !== null),
      'the landing echo did not ask for the turns above it',
    ).toEqual(['older']);
  });

  it('skips the layout read for the event its own pin wrote', async () => {
    // **#1710's actual property, guarded hermetically.** jsdom measures no
    // layout, but the stub's element counts what the column READ - and the
    // echo's whole saving is the read it does not make. The token-disabling
    // simplification arms the foot ASKED for, which `scrollTop` can never
    // equal, so no event matches it again: the echo reads the layout back and
    // this counts it.
    const server = stub();
    await draw(server);
    await settle();
    clear();

    // The content grows under a reader at the foot and the frame's pin rides
    // them down: the write moved, so its echo is on the way. The list's
    // measurement is the row's own report, so it moves with the element.
    setModel(TOTAL + 80);
    setElement(TOTAL + 80, VIEWPORT);
    server.frame();
    flushSync();
    const before = layout.reads;
    await settle();

    // The frame's own pass reads the height once - the pin's land; the
    // post-layout look is the observer's now (#1734) - and the echo reads it
    // none. Were the echo read, this would count two.
    expect(layout.reads - before, 'a read the frame did not owe').toBe(1);
  });

  it('follows again when a parked reader comes back inside the notch', async () => {
    // **The notch is the follow's boundary, not the exact end** (the rule the
    // book states): a reader who comes back within 48px of the bottom is at
    // the latest as far as the column's own gate is concerned, so the follow
    // comes back with them and the next append lands.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // Four pixels short, on the reader's own down-wheel: inside the notch.
    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: 120 }));
    readerAt(FOOT - 4);
    await settle();

    expect(following(server), 'the notch resumed the follow').toBe(true);
    expect(document.querySelector('button.follow'), 'the circle goes with it').toBeNull();
    expect(element.offset, 'and the column holds the foot').toBe(FOOT);
  });

  it('leaves a parked reader past the notch alone', async () => {
    // The control: the same return, sixty pixels short, is a reader reading.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    readerAt(FOOT - 60);
    server.frame();
    await settle();

    expect(pinned(), 'past the notch the reader is reading').toEqual([]);
    expect(following(server), 'and the follow stays off').toBe(false);
  });

  it('does not re-arm on a machine move that lands inside the notch', async () => {
    // **The notch re-arm is the READER's, not the layout's.** A correction
    // that happens to land inside the notch leaves the column standing at the
    // foot (`footHeld`), and that must not turn a parked reader into a
    // follower: there is no returning hand behind it.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // No wheel, no touch, no key: the column itself moved.
    list()?.scrolledTo(FOOT - 20, TOTAL, VIEWPORT);
    await settle();

    expect(following(server), 'a layout move does not re-arm the follow').toBe(false);
    expect(pinned(), 'and nothing lands the foot').toEqual([]);
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

  it('does not disarm on a move too small for a reader, with no input behind it', async () => {
    // **The drift's own arm.** A re-measure nudges the scroll by a couple of
    // pixels with no reader behind it, and judging that as the reader turned
    // the follow off - every growth after that left them further behind.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a couple of pixels is not a reader moving').toEqual([PIN]);
  });

  it('re-arms when a wheel was heard and the clamp lands at the foot carrying a shrink', async () => {
    // **The tokened half.** A wheel-up is heard, then the browser clamps the
    // reader DOWN to the foot the content has after a shrink - the event
    // carries the shrink and the offset FELL, so the direction reads away and
    // the token is the only thing that makes the event theirs. (The old shape
    // this test used to dispatch read as a return; the down-scroll return has
    // its own pin below.)
    const server = stub();
    await draw(server);
    readerAt(FOOT - 100);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: -120 }));
    const shrunk = TOTAL - 120;
    setElement(shrunk, VIEWPORT);
    const foot = Math.floor(shrunk) - VIEWPORT;
    list()?.scrolledTo(foot, shrunk, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the tokened clamp re-armed the follow').toEqual([
      { asked: Math.floor(shrunk), landed: foot },
    ]);
  });

  it('re-arms when the reader comes back DOWN to the foot in a shrinking event', async () => {
    // **Ved's gesture verbatim** (2026-10-11): "I scroll up and come back
    // down... the arrow does not go until I press it". A down-scroll arms no
    // up-capable token - wheel-up, ArrowUp, PageUp, Home and shift-space are
    // what do - so the return is read from the event's own direction: it
    // moved the reader toward the foot. Without this the fix covered only
    // the clamp shape and his own gesture still stuck the arrow.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // The return, landing at the foot in an event that carries a shrink - a
    // row settling to its drawn height as the scroll lands.
    const shrunk = TOTAL - 120;
    setElement(shrunk, VIEWPORT);
    const foot = Math.floor(shrunk) - VIEWPORT;
    list()?.scrolledTo(foot, shrunk, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the down-return re-armed the follow over the shrink').toEqual([
      { asked: Math.floor(shrunk), landed: foot },
    ]);
  });

  it('keeps an untokened shrink away from the foot from moving the follow', async () => {
    // **The clamp guard's other half, and the control for both tests above.**
    // A shrink nobody made with the reader short of the foot: nothing moves
    // either way. Re-arming there would carry a reader back down with
    // whatever arrived, which is the mistake the shrink skip exists for.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    const shrunk = TOTAL - 120;
    setElement(shrunk, VIEWPORT);
    // A hundred short of the foot: past the 48px notch, which is where a
    // reader is reading rather than standing at the latest.
    list()?.scrolledTo(Math.floor(shrunk) - VIEWPORT - 100, shrunk, VIEWPORT);
    flushSync();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'nothing moved').toEqual([]);
  });

  it("disarms on the reader's own move at any distance", async () => {
    // The other half of the rule: when the reader's own wheel was just heard,
    // even a two-pixel move is theirs, and the follow lets them go.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: -120 }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'their own wheel moved them, at any distance').toEqual([]);
  });

  it('does not arm on a wheel that can only move the reader down', async () => {
    // **A gesture that cannot move the reader up cannot have moved them up.**
    // A wheel-down at the foot scrolls nothing, and arming on it would excuse
    // the next adjuster write as the reader's - the drift in a narrower window.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: 120 }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a wheel-down did not move them').toEqual([PIN]);
  });

  it('hears the press that starts a scrollbar drag', async () => {
    // **The gesture the wheel, touch and keys cannot name.** A scrollbar drag
    // fires none of them, so its moves were read only past `DISARM_SLACK`
    // above the pin's landing - and while the follow is on, the pin re-writes
    // the foot on every arriving update, so a slow drag has to out-run the pin
    // before the disarm can land (Ved, 2026-10-16: "I have to make an effort
    // to actually move it up, and it is still fighting me"). The press that
    // starts the drag lands on the pane's own box - the scrollbar is not a row
    // - so it is heard where the drag begins.
    //
    // **The stub stands in for the geometry the browser computes.** The pane
    // is given a width and the press a `clientX` past it, in the scrollbar's
    // strip; jsdom reports `offsetX` as `clientX`, so the values carry what a
    // browser measures - on the live seat with real input, the bar reads 854
    // against 850 in Chromium and 934 against 930 in WebKit. What the stub
    // cannot model is the fall-through below (its rows hang directly off the
    // pane, with no `pointer-events` state), so that case is the one the two
    // clauses divide on.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    element.width = 400;
    const pane = document.querySelector('.conv');
    pane?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 500 }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'their own hand on the scrollbar moved them').toEqual([]);
  });

  it('does not arm on a press that lands on a row', async () => {
    // The control for the press above: a click inside the conversation is not
    // a hand on the scrollbar, and a click that shifts the layout by a couple
    // of pixels is the adjuster the slack exists for. Its offset is in the
    // content box, so the target clause is the one that has to refuse it.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    element.width = 400;
    const row = document.querySelector('.conv .turn');
    row?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 500 }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a press on a row is not a scrollbar press').toEqual([PIN]);
  });

  it('does not arm on a press inside the content box while the list is scrolling', async () => {
    // **Virtua's `pointer-events: none` window.** While the list is scrolling
    // it takes the content out of hit-testing for 150ms after the last scroll,
    // so a press on a ROW then arrives with the PANE as its target - measured
    // in Chromium (offset 200 against a clientWidth of 850) - and the offset
    // is the only thing that tells it from a hand on the bar. Without the
    // clause the click arms the token, and a nudge the slack exists to absorb
    // turns the follow off for a reader who never touched the bar.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    element.width = 400;
    const pane = document.querySelector('.conv');
    pane?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 300 }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a fall-through press is not a bar press').toEqual([PIN]);
  });

  it('holds a pressed scrollbar past the token window, until the release', async () => {
    // **The press-then-pause drag.** A reader who presses the thumb, aims for
    // a second and only then drags gets nothing from a 250ms token, and a drag
    // fires no second pointerdown to re-arm - measured in both engines, where
    // a real drag delivers pointerdown and pointerup and NO moves between
    // them. So the press holds until the release does.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      element.width = 400;
      const pane = document.querySelector('.conv');
      pane?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 500 }));
      // Past the window the press armed on its own.
      vi.advanceTimersByTime(300);

      readerAt(FOOT - 2);
      // A size change is the observer's pass, which is where a pin runs when
      // the follow is still on: with the press held, the disarm above means
      // there is none to run.
      resized();
      flushSync();

      expect(pinned(), 'a held press is the reader, however long it waits').toEqual([]);

      // The release gives the hand back: with the follow on again (the reader
      // pressing the way-back, which is how they return), the same move is the
      // nudge the slack exists for and the follow takes them down again.
      window.dispatchEvent(new MouseEvent('pointerup'));
      const back = document.querySelector('button.follow');
      back?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      flushSync();
      clear();

      readerAt(FOOT - 2);
      resized();
      flushSync();

      expect(pinned(), 'and a released bar is not a hand').toEqual([PIN]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('gives the hand back when the window is lost with no release', async () => {
    // **The press let go outside the app.** A pointerup released over another
    // window belongs to that window, so nothing arrives here - and a hold left
    // standing makes every later nudge the reader's for the life of the seat:
    // it turns the slack off and lets a clamp carry a parked reader back to
    // the foot. The token this replaced self-healed in 250ms; the hold cannot,
    // so every exit clears it - the release, the window going, the attachment
    // ending, the seat changing.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      element.width = 400;
      const pane = document.querySelector('.conv');
      pane?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 500 }));
      vi.advanceTimersByTime(300);

      // Lost with no pointerup, as a release over another app is.
      window.dispatchEvent(new Event('blur'));

      readerAt(FOOT - 2);
      resized();
      flushSync();

      expect(pinned(), 'a lost window is not a hand').toEqual([PIN]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('does not carry a hold across a list that unmounted mid-press', async () => {
    // **The list goes while the hand is down.** An occupant swap empties the
    // conversation, so the pane - and with it the window listeners - tears
    // down under the press, and the release that would have cleared the hold
    // lands after they are gone. The component instance survives, so the flag
    // is still standing when the new occupant's list comes back: every later
    // nudge reads as the reader's, the slack stops protecting them and a clamp
    // carries them to the foot.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      element.width = 400;
      const pane = document.querySelector('.conv');
      pane?.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, clientX: 500 }));
      vi.advanceTimersByTime(300);

      // The swap: the held turns go, the list unmounts with the hold set, and
      // the release arrives where no listener is left to hear it.
      server.replaced();
      await vi.advanceTimersByTimeAsync(0);
      flushSync();
      window.dispatchEvent(new MouseEvent('pointerup'));

      // The new occupant's own page, and the list with it. Its landing gets
      // its painted frames before the records are cleared, or the landing's
      // own pin would land among the ones under test.
      server.page([turn('t1'), turn('t2')]);
      await vi.advanceTimersByTimeAsync(120);
      flushSync();
      clear();

      readerAt(FOOT - 2);
      flushSync();

      // The disarm is the thing the stuck hold would cause, and the way-back
      // button is the disarm's own mark: with the hold cleared, the move is
      // the nudge the slack absorbs and the follow stays on. (The pin itself
      // is not the assertion here: a remount leaves the harness's stale
      // observer firing beside the new one, so a count would read two.)
      expect(
        document.querySelector('button.follow'),
        'a handle lost with the pane is not a hand',
      ).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("disarms on the reader's own key at any distance", async () => {
    // The keys are the vocabulary the view this client replaced used: the
    // ones that scroll the column up are the ones that may let the follow go.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document
      .querySelector('.conv')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp' }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'their own key moved them, at any distance').toEqual([]);
  });

  it('does not arm on a key that cannot scroll the column up', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new KeyboardEvent('keydown', { key: 'a' }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a letter key was not a scroll').toEqual([PIN]);
  });

  it('does not arm on a space without shift, which scrolls the column down', async () => {
    // The down-key class the narrowing exists for: a plain space pages the
    // column toward the end, so it cannot have moved the reader up.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new KeyboardEvent('keydown', { key: ' ' }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a plain space was not the reader moving up').toEqual([PIN]);
  });

  it('disarms on shift-space, the key that scrolls the column up', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document
      .querySelector('.conv')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', shiftKey: true }));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'shift-space scrolled them up, at any distance').toEqual([]);
  });

  it("disarms on the reader's own touch at any distance", async () => {
    // Touch arms whole - a touch carries no direction - and Android is the
    // other half of this client.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new Event('touchstart'));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'their own touch moved them, at any distance').toEqual([]);
  });

  it('disarms on a moving touch too', async () => {
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new Event('touchmove'));
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'a moving touch is still their hand').toEqual([]);
  });

  it('disarms just past the notch', async () => {
    // The boundary the slack draws: inside it a move is not the reader's.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    readerAt(FOOT - 49);
    server.frame();
    await settle();

    expect(pinned(), "past the notch the move is the reader's").toEqual([]);
  });

  it('keeps following at exactly the notch', async () => {
    // The other edge of the same boundary: 48 is inside it, so a flip at the
    // comparison now dies here.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    readerAt(FOOT - 48);
    server.frame();
    await settle();

    expect(pinned(), 'the notch is a distance a move must exceed').toEqual([PIN]);
  });

  it('keeps the input window open across its own events', async () => {
    // The window is a time, not a one-shot the first event spends: an event
    // of the same gesture that is not the move does not take it away.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: -120 }));
    // An event of the same gesture that lands on the pin rather than moving.
    list()?.scrolledTo(FOOT, TOTAL, VIEWPORT);
    flushSync();
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'the window held across the gesture, not for one event').toEqual([]);
  });

  it('keeps the token through an event that reads it without disarming', async () => {
    // A second place a token could be spent: the read in the disarm arm
    // itself. This event sits at the pin while the content below it has
    // grown, so it reads the token and does not disarm - and the move after
    // it still must.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: -120 }));
    setElement(TOTAL + 200, VIEWPORT);
    list()?.scrolledTo(FOOT, TOTAL + 200, VIEWPORT);
    flushSync();
    readerAt(FOOT - 2);
    server.frame();
    await settle();

    expect(pinned(), 'the token survived the read that did not disarm').toEqual([]);
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

    expect(pinned(), 'the new conversation opens at its foot').toEqual([PIN]);
  });

  it('lands a kept seat at its foot after the reader parked on another', async () => {
    // **The list survives the switch, so the entry's own arm is the only
    // thing that can put the reader at the foot.** Visited seats are kept,
    // and the list is not re-created for a seat change - so returning to one
    // runs no mount reset, and a reader parked on the seat they are leaving
    // would otherwise open the kept conversation wherever that number falls.
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

    // Visit the other seat and let its page land, then park on it. The offset
    // has to MOVE for the reader's place to be read: the switch left it at 0.
    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    readerAt(100);
    await settle();
    clear();

    // Back to the first, whose conversation is kept and whose list is the one
    // already mounted.
    seat.set(LEAD);
    flushSync();
    await settle();

    expect(pinned(), 'the kept seat opens at its foot').toEqual([PIN]);
    expect(element.offset, 'and the reader is there').toBe(FOOT);
  });

  it('opens a kept conversation at the element foot when the model lags on the way back', async () => {
    // **The entry's own lag.** A kept conversation's rows are already in the
    // element, and the list measures them a pass later - so on the way back
    // the element's scrollHeight runs ahead of the model's size. A first pin
    // that holds at the model lands the reader near the top of the
    // conversation and then follows the growth down in view: "it starts
    // somewhere up and then it scrolls down to the bottom". An open PLACES
    // its reader rather than following a foot that moved, so the first pin
    // must write the element's own foot, not the model's short one.
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
    readerAt(FOOT);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // The way back: the element counts the kept conversation's rows before
    // the list has measured them.
    setElement(TOTAL + 2000, VIEWPORT);
    seat.set(LEAD);
    flushSync();
    await settle();

    const grown = Math.floor(TOTAL + 2000);
    expect(pinned(), "the entry places the reader at the element's foot").toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
    expect(element.offset, 'and the reader is there, not near the top').toBe(grown - VIEWPORT);
  });

  it('keeps placing a switch entry on the pass after it, while the model still lags', async () => {
    // **The settle is more than one pass.** A kept conversation's rows are in
    // the element while the list measures them a pass later, so the pin after
    // the entry must write the element's foot again rather than hold at the
    // model's short one and walk the reader down as the measurement lands.
    // The entry tests above see only the first pin, which the switch's own
    // clear already explains; this is the pass after it.
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
    readerAt(FOOT);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // The way back: the element counts the kept conversation's rows before
    // the list has measured them.
    setElement(TOTAL + 2000, VIEWPORT);
    seat.set(LEAD);
    flushSync();
    await settle();
    clear();

    // One more frame, which moves neither measured size: what lands now can
    // only be the settle re-measuring the same lagging model.
    server.frame();
    await settle();

    const grown = Math.floor(TOTAL + 2000);
    expect(pinned(), "the settle places the reader at the element's foot again").toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
    expect(element.offset, 'and the reader never left it').toBe(grown - VIEWPORT);
  });

  it('never pulls the reader back to the model after an entry placed past it', async () => {
    // **The hold is a ceiling on a forward write, not a drag.** An entry
    // places at the element's foot while the model still lags, and rows
    // arriving below grow the element further. Holding at the model's short
    // foot would pull the reader up and then glide them back down as it
    // measured - the entry walk again, one pass later.
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
    readerAt(FOOT);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // The model is AHEAD at the entry, which is what settles the mount and
    // makes the column a follower from here on.
    setModel(TOTAL + 3000);
    setElement(TOTAL + 2000, VIEWPORT);
    seat.set(LEAD);
    flushSync();
    await settle();
    const placedAt = Math.floor(TOTAL + 2000) - VIEWPORT;
    expect(element.offset, 'precondition: the entry placed the reader').toBe(placedAt);
    clear();

    // The model then measures a shorter end than the element already holds -
    // the lag the hold is for. The reader is past the model's foot and stays.
    setModel(TOTAL);
    server.frame();
    await settle();

    expect(
      pinned().filter((pin) => pin.landed < placedAt),
      'the hold did not pull the reader up to the model',
    ).toEqual([]);
    expect(element.offset, 'and the reader stayed where the entry placed them').toBe(placedAt);
  });

  it('places a first-ever entry at the foot instead of gliding down to it', async () => {
    // **An empty column is not a settled one.** On the mount's first pass the
    // element and the list both read zero, so "the model reached the
    // element's end" was trivially true and the first content frame took the
    // follow glide - a conversation opened for the first time travelled from
    // the top of the column down to its foot.
    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    const server = stub();
    clear();
    clearObservers();
    // A live empty column, not an absent one: the element cannot report less
    // than its own height, so "nothing to scroll" is `max === 0`.
    setMeasured(VIEWPORT, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.session('s1');
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);

    // The rows draw before the list has measured them.
    setElement(TOTAL, VIEWPORT);
    server.frame();
    await settle();

    // Under the old condition the empty pass settled, so this pass held at the
    // model's foot and left the reader at the top of the column.
    expect(element.offset, 'the first entry placed the reader at the foot').toBe(FOOT);

    // And the list catching up ends the settle without moving them.
    setModel(TOTAL);
    server.frame();
    await settle();
    expect(element.offset, 'and the settle kept them at the foot').toBe(FOOT);
  });

  it('opens a plain new conversation at the foot for a parked reader', async () => {
    // **A plain swap unmounts the list, and what mounts opens following.**
    // The rows are new and the reader has no place in them, so the column
    // lands at the foot - the list's own attach is the only thing that says a
    // fresh viewport is at its start rather than wherever the old one was
    // left.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.replaced();
    await settle();
    clear();

    server.page([turn('n1'), turn('n2')]);
    await settle();

    expect(pinned(), 'the new conversation opens at its foot').toEqual([PIN]);
  });

  it('goes back to the place a seat was left at when the reader switches to it', async () => {
    // **A saved place belongs to the conversation, not to the visit.** A
    // reader who parks on a seat and moves to another one comes back to the
    // row they left, the way the view this client replaced did with its
    // per-session `auto_scroll`.
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
    server.page([spoken('t1'), spoken('t2')]);
    await settle();
    layOut(40);

    // The reader scrolls up on the seat they are on, then leaves it.
    readerAt(50);
    await settle();
    expect(element.offset, 'precondition: the reader parked').toBe(50);
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // And comes back: the kept conversation opens at the row they left. The
    // rows this seat's conversation draws are new elements, and jsdom lays
    // nothing out.
    seat.set(LEAD);
    flushSync();
    layOut(40);
    await settle();
    await settle();

    expect(list(), 'the kept conversation is drawn again').not.toBeNull();
    expect(element.offset, 'the switch back reopened the place the seat was left at').toBe(50);
    expect(
      pinned().filter((pin) => pin.landed === Math.floor(TOTAL) - VIEWPORT),
      'a restored place is not a foot to pin',
    ).toEqual([]);

    // **And a seat left AT its foot still re-pins on the way back.** This leg
    // is that path's own regression guard: the conversation's follow is
    // already on, so the re-arm writes the same record, and the arriving
    // record's publish is what re-runs the pass.
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

    expect(pinned(), 'a return to a seat left at its foot lands there too').toEqual([PIN]);
  });

  it('stands the scroll handler aside while a return is placing its reader', async () => {
    // **A return's own scroll events are not the reader's.** The list reports
    // its position as the kept conversation is swapped in, before its rows
    // measure - and a short element reads as the foot. Without the guard the
    // handler re-arms the follow over the place being restored and deletes it
    // before the restore's first frame runs, so the reader lands at the foot
    // instead of the row they left.
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
    server.page([spoken('t1'), spoken('t2')]);
    await settle();
    layOut(40);

    readerAt(50);
    await settle();
    expect(element.offset, 'precondition: the reader parked').toBe(50);
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    seat.set(LEAD);
    flushSync();
    // The transient the returning mount reports: a short element at its start,
    // which the foot arithmetic reads as the end. Its rows then measure and
    // the element takes its real height before the restore's frame runs.
    setElement(VIEWPORT, VIEWPORT);
    list()?.scrolledTo(0, VIEWPORT, VIEWPORT);
    flushSync();
    setElement(TOTAL, VIEWPORT);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the return kept the place the reader left').toBe(50);
    expect(
      pinned().filter((pin) => pin.landed === 0),
      'the mount event did not pin the foot',
    ).toEqual([]);
  });

  it('keeps the place when a return writes against a short element', async () => {
    // **The restore's own write comes back as an event too.** A returning
    // mount can be short while its big turn has not measured - the element
    // reports the viewport's height, not the transcript's - so the write to
    // the saved row clamps to the element's end and the event it fires reads
    // as the foot. Without the write's own token the handler re-arms the
    // follow over the place being restored and deletes it, and the reader is
    // carried down to the foot as the turn measures: "it starts somewhere up
    // and then it scrolls down to the bottom" (Ved, 2026-11-05).
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
    server.page([spoken('t1'), spoken('t2')]);
    await settle();
    layOut(40);

    readerAt(50);
    await settle();
    expect(element.offset, 'precondition: the reader parked').toBe(50);
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([turn('o1'), turn('o2')], OTHER);
    await settle();
    clear();

    // The way back, short: the element reports only its viewport while the
    // restore's frame runs, so the write to the saved row clamps.
    setElement(VIEWPORT, VIEWPORT);
    seat.set(LEAD);
    flushSync();
    // **The restore's own passes are drained before the element grows.**
    // `settle` pumps a fixed count of frames, and a loaded machine pushes the
    // restore's last pass past them - so the write against the short element
    // lands after `setElement` below instead of before it, and the case stops
    // exercising the short write it is named for.
    await drained();

    // The element then measures to its real height.
    setElement(TOTAL, VIEWPORT);
    layOut(40);
    // **The growth is the browser's event, not a frame the test can count.**
    // Deliver it as the ResizeObserver does: without it the place is only
    // restored if a deferred pass happens to land on the right frame, which
    // is the order the gate's load changes.
    resized();
    await settle();

    // The short element has nowhere to scroll, so the restore's write lands at
    // its start and is recorded as a pin; what matters is that its own echo was
    // consumed and the place survived to the full measurement.
    expect(element.offset, 'the return kept the place the reader left').toBe(50);
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
    // draw now lands on a painted frame rather than at the write (#1670), and
    // the passes that find nothing moved no longer pin at all (issue #1710) -
    // what the test is about is WHICH foot every pin asks for: the element's
    // numbers, never the model's longer ones. A pin asking for anything else
    // fails whichever pass it came from.
    const pins = pinned();
    expect(pins.length, 'the column pinned, over its passes').toBeGreaterThanOrEqual(1);
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

    // The first row grows by 200, so the reader's row starts 200 further
    // down; the observer reports the size change and restores, the way a
    // browser fires it (#1734: the pass is the observer's, not a per-frame
    // rAF's).
    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, "the reader's row carried them down with it").toBe(250);
  });

  it('takes the place again once the list has re-windowed for a jump', async () => {
    // **A jump can outrun the list's window.** The capture reads the rows the
    // last window left drawn, finds none under the edge, and asks again once
    // the list has re-windowed - and only two frames that agree are a place.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();
    places.forget(subjectKey({ session: LEAD }));

    // Every drawn row sits below the top edge, further than the room: the
    // capture finds no row to hold.
    const root = document.querySelector('.conv');
    const rows = [...(root?.querySelectorAll('.turn [data-k]') ?? [])];
    rows.forEach((row) => {
      row.getBoundingClientRect = () => ({
        top: 2000,
        bottom: 2040,
        height: 40,
        width: 0,
        left: 0,
        right: 0,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      });
    });
    const placeRow = (index: number, top: number): void => {
      const row = rows[index];
      if (row === undefined) return;
      row.getBoundingClientRect = () => ({
        top,
        bottom: top + 40,
        height: 40,
        width: 0,
        left: 0,
        right: 0,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      });
    };
    readerAt(50);
    expect(
      places.of(subjectKey({ session: LEAD })),
      'no row under the edge is no place',
    ).toBeUndefined();

    // **Two frames have to agree, and this gives them different answers.**
    // The layout is a jump's own correction: row A takes the edge for one
    // frame and row B the next two, and only B - the row two frames kept - is
    // the reader's place. A capture that took the first row it saw records A.
    placeRow(0, 0);
    await frame();
    placeRow(0, -40);
    placeRow(1, 0);
    await frame();
    // The second frame at B, which is the agreement.
    await frame();

    expect(
      places.of(subjectKey({ session: LEAD }))?.anchor.key,
      'the retry took the row two frames agreed on, not the first it saw',
    ).toBe(rows[1]?.getAttribute('data-k') ?? undefined);
  });

  it('gives up a capture that never settled instead of restoring the old row', async () => {
    // **A jump whose layout never settles.** Every drawn row sits below the
    // top edge for longer than the capture's budget, so no row is the place.
    // The anchor it still holds names the pre-jump row, and left standing the
    // next pass would restore the reader back to it - undoing the jump.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // Rows that draw, so the reader has a row to park on.
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    expect(
      places.of(subjectKey({ session: LEAD })),
      'precondition: the reader parked on a row',
    ).toBeDefined();
    clear();

    const root = document.querySelector('.conv');
    const rows = [...(root?.querySelectorAll('.turn [data-k]') ?? [])];
    rows.forEach((row) => {
      row.getBoundingClientRect = () => ({
        top: 2000,
        bottom: 2040,
        height: 40,
        width: 0,
        left: 0,
        right: 0,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      });
    });
    vi.mocked(noteScrollMove).mockClear();
    readerAt(10);
    for (let at = 0; at < 8; at += 1) await frame();

    expect(
      places.of(subjectKey({ session: LEAD })),
      'the capture gave up rather than holding the old row',
    ).toBeUndefined();
    // **The log says which give-up it was.** The desktop shell names the
    // record's `name`, so the marker crossing the page -> shell boundary is
    // what a reader of the log has to see.
    expect(noteScrollMove, 'the give-up is written to the log').toHaveBeenCalledWith(
      expect.objectContaining({ name: 'capture-gave-up' }),
    );

    // The layout comes back and reports itself: a stale anchor would restore
    // the pre-jump row here.
    layOut(40);
    resized();
    await settle();

    expect(element.offset, 'the reader stays where they jumped to').toBe(10);
  });

  it('restores the row of a reader whose follow is on but not pinning', async () => {
    // **The other half of the third state.** A follow can be on while the
    // column holds position; the observer's pass must not read that flag
    // alone and skip the restore, or a parked row moves under the reader
    // whenever the layout above them changes.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();

    layOut(40);
    readerAt(50);
    await settle();
    // The follow is turned on by an arrival while the reader stays parked -
    // the state the pin's gate holds.
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();
    clear();

    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, "the reader's row is still their place").toBe(250);
  });

  it('holds the anchor of a following reader who is not at the foot', async () => {
    // **The third state through the pass no observer reports.** An equal-height
    // reorder above the reader is the anchor effect's own trigger, and it used
    // to skip on the follow flag alone - so a following reader whose pin the
    // gate refuses (a handoff after an unpaid send is one wire path there) had
    // neither the pin nor the restore, and the column moved under them.
    const server = stub();
    await draw(server);
    server.page([spoken('a'), spoken('b'), spoken('c'), spoken('d')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();
    clear();

    // Every row the same height, and the anchor's turn and its neighbour
    // swap: no size report, so only the anchor pass can hold the reader.
    server.page([spoken('a'), spoken('c'), spoken('b'), spoken('d')]);
    flushSync();
    layOut(40);
    await settle();

    expect(element.offset, "the reader's row carried them through the reorder").toBe(90);
  });

  /**
   * **The move no observer event reports** (#1734): an equal-height reorder
   * puts the anchor row somewhere else with every size unchanged - the
   * ResizeObserver fires nothing, and a height check sees the same total. The
   * parked pass is scheduled on exactly this: the signature of the keys above
   * the anchor, a data comparison over the drawn rows that needs no layout
   * read.
   */
  it('holds a parked reader through an equal-height reorder', async () => {
    const server = stub();
    await draw(server);
    // Keys of their own: `draw` has already paged t1/t2, and a page reusing
    // those keys hands the list DOM it has already drawn.
    server.page([spoken('a'), spoken('b'), spoken('c'), spoken('d')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The reorder as a page that carries it would: the anchor's turn and its
    // neighbour swap places, so every rect stays 40px tall and the total does
    // not move - the keys and the boxes travel together.
    server.page([spoken('a'), spoken('c'), spoken('b'), spoken('d')]);
    flushSync();
    layOut(40);
    await settle();

    expect(element.offset, "the reader's row carried them through the reorder").toBe(90);
  });

  /**
   * **A re-keyed row is still the reader's place.** The read's copy of an
   * exchange can carry the ids the live frames did not, so the rows stay where
   * they are while their keys change - and the anchor, held by the old key,
   * missed on every later restore and left the reader behind (#1834). A miss
   * the conversation no longer holds re-captures from the viewport.
   */
  it('re-captures the anchor when a page re-keys the row the reader holds', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // One turn of two rows, both keyed by their position: no frame carries an
    // id yet.
    server.page([
      {
        key: 't9',
        messages: [idless('user', 'first line'), idless('user', 'second line')],
      },
    ]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The read's copy of the same exchange, carrying the ids: same rows, same
    // order, new keys.
    server.page([
      {
        key: 't9',
        messages: [identified('u1', 'user', 'first line'), identified('u2', 'user', 'second line')],
      },
    ]);
    flushSync();
    layOut(40);
    await settle();

    // The move a stale anchor could no longer follow.
    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, "the reader's row carried them down with it").toBe(250);
  });

  /**
   * **A row taken away is re-captured too, not only a re-keyed one.** The
   * fold can drop the row the reader holds; their place is still whatever
   * row holds the viewport's top edge, and leaving the anchor on a key the
   * conversation no longer draws strands it (#1834).
   */
  it('re-captures the anchor when the row it holds is taken away', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('a'), spoken('b'), spoken('c'), spoken('d')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();

    // The row the reader holds is gone, and the rows after it close up.
    document.querySelectorAll('.conv .turn [data-k]')[1]?.remove();
    layOut(40);
    resized();
    await settle();

    // A later move above them is carried from the re-captured place.
    layOut(40, 240);
    resized();
    await settle();

    expect(element.offset, "the reader's row carried them down with it").toBe(250);
  });

  /**
   * **A second publish before the paint must not eat the restore.** A
   * reorder's pass is owed on the paint after the detecting run, and the
   * effect's teardown cancels a pending rAF on every re-run - so a publish
   * landing in that window (every synchronous fold publish does: a page, a
   * refusal, a follow decision) used to drop the debt with the signature
   * already stamped, and no pass ever fired again.
   */
  it('still restores when a second publish lands before the paint', async () => {
    const server = stub();
    await draw(server);
    server.page([spoken('a'), spoken('b'), spoken('c'), spoken('d')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    server.frame();
    await settle();
    clear();

    server.page([spoken('a'), spoken('c'), spoken('b'), spoken('d')]);
    // The second publish, in the window between the first one's schedule and
    // the paint the pass was for: no tick between them.
    server.page([spoken('a'), spoken('c'), spoken('b'), spoken('d')]);
    flushSync();
    layOut(40);
    await settle();

    expect(element.offset, "the reader's row carried them through both publishes").toBe(90);
  });

  /**
   * **The negative half of the acceptance: a frame that changed nothing above
   * the anchor pays for no pass.** The parked pass's whole cost is the row box
   * it reads, so the observable is the box reads themselves - the same number
   * the perf rig counts - and a slice running wider than "above the anchor"
   * would schedule for churn that moved nothing above them.
   */
  it('reads no row box for a frame that changed nothing above the anchor', async () => {
    const server = stub();
    await draw(server);
    server.page([spoken('a'), spoken('b'), spoken('c'), spoken('d')]);
    await settle();
    layOut(40);
    readerAt(50);
    await settle();
    clear();
    rectReads = 0;

    // The stream's own frame: it appends under the newest turn, so nothing
    // above the parked anchor moved.
    server.frame();
    await settle();

    expect(rectReads, 'a quiet frame paid for a parked pass').toBe(0);
  });

  /**
   * **A size change with an ask in flight is the compensation's business.**
   * Near the top a page of older turns is asked for, and while that ask is out
   * the restore stands aside - the list is holding the reader by its own
   * compensation, and a row above them growing is not theirs to compensate.
   * (Before the guard this path restored anyway: the offset would read 250
   * here.)
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
   * Without that drain the count outlives the ask, `compensating` stays on for
   * the life of the seat, and this PR's own hold quietly stops working: the
   * offset left at 50 where the row above the reader had moved it to 250.
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
   * its forgotten ask, the column keeps holding one, `compensating` stays on
   * for the life of the seat, and the observer's restore never runs for a
   * parked reader (measured: the offset left at 50 where the row above them
   * had moved it to 290).
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

    expect(pinned(), 'a prompt is where the reader wants to be, wherever they were').toEqual([PIN]);
  });

  it("keeps a send's arm through the frame that follows it", async () => {
    // **The turn's next frame must not spend the arm.** A send arms the
    // follow, and the frame that lands before the pin's pass folds through
    // the same append path - rebuilding the arm from that frame's own
    // `source` alone would drop the reader's send and leave a parked reader
    // where they were.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.promptThenFrame();
    await settle();

    expect(pinned(), "the reader's own send still lands the foot").toEqual([PIN]);
  });

  it('brings the reader back for their own send the CLI queued', async () => {
    // A prompt the CLI queues never folds as a user unit, so a mid-turn send
    // is announced by the queue alone - and the pile's card IS that message's
    // row, so their own words still put the foot under their eyes.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.queued('you');
    await settle();

    expect(pinned(), 'their own queued send still comes back to the foot').toEqual([PIN]);
  });

  it('does not bring a parked reader back for a prompt that is not theirs', async () => {
    // **The #1614 snap, named by its arm.** The same `prompt_queued` frame
    // announces a cron fire, a peer message, a delivery and forge's own
    // continuation - none of them the reader's words - and a re-arm for one
    // of them carries a reader parked above the foot to the bottom on the
    // next pass (measured live: a cron-sourced queue frame moved the reader
    // 6268 -> 9208 mid-stream). Only `source` tells the two apart.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.queued('cron');
    await settle();

    expect(pinned(), 'a cron fire is not the reader words and may not carry them').toEqual([]);
  });

  it('does not bring a parked reader back for a prose turn the core forged', async () => {
    // **The append half of #1614's snap, named by its arm.** forge's own
    // machinery writes prose as a user row - an auto-continue, a worker kick,
    // a board notice - on the same frame a person's send uses, and a re-arm
    // for one of them carries a reader parked above the foot to the bottom on
    // the next pass. Only the frame's `source` tells the two apart.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.forged('forge');
    await settle();

    expect(pinned(), "forge's own words are not the reader's and may not carry them").toEqual([]);
  });

  it('does not bring a parked reader back when a forged prompt leaves the queue', async () => {
    // The drain is the append path's other arm: a prompt the CLI queues draws
    // its row when the CLI takes it, and a re-arm there carries the reader
    // just as a fresh frame does. Its sender is the queue announcement's, and
    // only `you` may arm.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.queued('forge');
    server.started();
    await settle();

    expect(pinned(), 'a queued continuation is forge talking to itself').toEqual([]);
  });

  it('brings the reader back for their own queued words that only the read carried', async () => {
    // **The drain's positive arm, and the one case nothing else covers.** A
    // client that reconnects onto a seat whose own prompt is still waiting
    // gets no `prompt_queued` frame - the pending backlog goes to the
    // first-ever subscriber alone - so the snapshot's queue is the only thing
    // that knows the words are this reader's press, and the echo it kept
    // across the drop is what says the row is this client's. The drain at
    // `started` is then the one place they can put the foot back under them.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // The press this client made before the drop: the echo carries its prompt
    // id, and the read's queue row names the same one.
    echoes.post(subjectKey({ session: LEAD }), 'what is this', false, 'said-once', echoes.mint());
    server.readQueue([{ uuid: 'said-once', source: 'you', text: 'what is this' }]);
    await settle();
    server.started();
    await settle();

    expect(pinned(), 'their own queued words come back to the foot off the read alone').toEqual([
      PIN,
    ]);
  });

  it('does not bring a parked reader back for queued words this client never pressed', async () => {
    // **The read's queue is not a press either.** A row the snapshot carries
    // under an id this client never minted is the reader's own words from
    // somewhere else - the phone's send while they read here - so it lands in
    // the badge rather than under their eyes.
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    server.readQueue([{ uuid: 'phone-queued', source: 'you', text: 'from the phone' }]);
    await settle();
    // The turn that held the queue ends, and the CLI takes the next prompt:
    // that started prompt opens the row the reader was away for.
    server.inflight(false);
    await settle();
    server.startedFor('phone-queued');
    await settle();

    expect(pinned(), "the phone's queued words may not carry the reader").toEqual([]);
    expect(badge(), 'and they ride the badge, as every turn they missed does').toBe('1');
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

  it('leaves the follow off on a clamp the column height made', async () => {
    // **The clamp with no content behind it.** The composer below collapses -
    // a dictation row going, a panel closing - and the column takes the room,
    // so the foot comes up under a reader parked just above it and the
    // browser clamps them down to it. The content height never moved, so the
    // shrink guard reads this as a reader arriving at the end, and the next
    // frame pins them back to a foot they never asked for.
    const server = stub();
    await draw(server);
    readerAt(FOOT - 60);
    await settle();
    clear();

    const roomier = VIEWPORT + 100;
    setElement(TOTAL, roomier);
    list()?.scrolledTo(TOTAL - roomier, TOTAL, roomier);
    await settle();
    clear();

    // The next frame lands a row below them, so a follow that re-armed has
    // somewhere to carry them to.
    setElement(TOTAL + 120, roomier);
    server.frame();
    await settle();

    expect(pinned(), 'a clamp the column height made is not a reader arriving at the end').toEqual(
      [],
    );
  });

  it("re-arms when the reader's own down gesture meets a room-gain clamp", async () => {
    // **The same clamp, with a hand behind it.** A down gesture arms no
    // up-token - it cannot be the hand that took the reader away - and the
    // foot rising past their target leaves the clamp's landing below the
    // offset they came from, so the event's direction reads away. Unless the
    // down gesture is heard as its own arm, the return is swallowed and the
    // way-back arrow sticks until pressed: the 2026-10-11 symptom, one clamp
    // over.
    const server = stub();
    await draw(server);
    readerAt(FOOT - 100);
    await settle();
    clear();

    document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: 120 }));
    const roomier = VIEWPORT + 200;
    setElement(TOTAL, roomier);
    list()?.scrolledTo(TOTAL - roomier, TOTAL, roomier);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'their own down gesture re-armed the follow across the clamp').toEqual([
      { asked: Math.floor(TOTAL), landed: Math.floor(TOTAL) - roomier },
    ]);
  });

  it('re-arms from a down key meeting a room-gain clamp', async () => {
    // **The key half of the aimed arm.** A down key is the same gesture as a
    // wheel-down through a different listener, and the branch is shared: this
    // is the sibling that dies if the keydown aim goes.
    const server = stub();
    await draw(server);
    readerAt(FOOT - 100);
    await settle();
    clear();

    document
      .querySelector('.conv')
      ?.dispatchEvent(new KeyboardEvent('keydown', { key: 'PageDown' }));
    const roomier = VIEWPORT + 200;
    setElement(TOTAL, roomier);
    list()?.scrolledTo(TOTAL - roomier, TOTAL, roomier);
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'their own down key re-armed the follow across the clamp').toEqual([
      { asked: Math.floor(TOTAL), landed: Math.floor(TOTAL) - roomier },
    ]);
  });

  it('lets an aimed gesture expire before the clamp lands', async () => {
    // **A sticky aim is the round-one bug wearing the hand's badge.** The
    // token is a 250ms window like the disarm's, and a hand heard long before
    // a clamp cannot make that clamp the reader's: without the expiry every
    // later room-gain clamp for the life of the seat re-arms.
    const server = stub();
    await draw(server);
    readerAt(FOOT - 100);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      document.querySelector('.conv')?.dispatchEvent(new WheelEvent('wheel', { deltaY: 120 }));
      vi.advanceTimersByTime(300);

      const roomier = VIEWPORT + 200;
      setElement(TOTAL, roomier);
      list()?.scrolledTo(TOTAL - roomier, TOTAL, roomier);
      flushSync();

      expect(pinned(), 'an expired aim is not a hand on this clamp').toEqual([]);
    } finally {
      vi.useRealTimers();
    }
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

    expect(pinned(), 'the end they are sitting at is followed again').toEqual([FITS]);
  });
});

/**
 * **The heights the follow pass never ran on.**
 *
 * A turn's row grows after the frame that grew it has published, the beat
 * hands its report row back with no publish at all, and the composer takes
 * room without one message changing - each a way the newest end ends up below
 * the fold. The size notification that reports one of them is also a thing a
 * browser drops under load (#1759 measured it).
 */
describe('the heights a turn ends at', () => {
  /** How much the finished turn's report row adds to the turn, in the test's own layout. */
  const ROW = 120;
  /** One assistant frame, which is where the running row's figures come from. */
  const spoke = (uuid: string): unknown => ({
    type: 'assistant',
    uuid,
    timestamp: '2026-10-01T06:00:00Z',
    message: {
      id: `m-${uuid}`,
      role: 'assistant',
      model: 'claude-opus-5',
      content: [{ type: 'text', text: 'working' }],
      usage: { input_tokens: 100, output_tokens: 20 },
    },
  });
  /** The reader's own message, which is what opens a turn. */
  const asked = {
    type: 'user',
    uuid: 'u-t2',
    message: { role: 'user', content: [{ type: 'text', text: 'go' }] },
  };
  /** The newest turn as the seat says it is running: its row is the pin's. */
  const live = { key: 't2', running: true, messages: [asked, spoke('a1')] };
  /** The same turn, settled by the core's own result. */
  const settled = {
    key: 't2',
    running: false,
    messages: [
      asked,
      spoke('a1'),
      {
        type: 'result',
        uuid: 'r1',
        duration_ms: 42_000,
        duration_api_ms: 20_000,
        total_cost_usd: 0.1,
        usage: {},
      },
    ],
  };

  /** A column whose newest turn is running, so its report row is the pin's. */
  async function running(server: ReturnType<typeof stub>): Promise<void> {
    await draw(server);
    server.inflight(true);
    server.page([turn('t1'), live]);
    await settle();
    expect(
      document.querySelectorAll('details.turninfo'),
      'the running row is pinned, not drawn in the turn',
    ).toHaveLength(0);
  }

  /**
   * Finish the running turn, and let the beat hand its row back to the turn.
   *
   * `grow` is the layout the hand-over makes: the turn is taller by the row
   * the moment the beat lets it go, so it is written between the timer and
   * the flush that runs the pass - the way a browser paints the row into the
   * turn and then lays it out.
   */
  async function beat(server: ReturnType<typeof stub>, grow = 0): Promise<void> {
    server.inflight(false);
    server.page([turn('t1'), settled]);
    await vi.advanceTimersByTimeAsync(34);
    flushSync();
    expect(
      document.querySelectorAll('details.turninfo'),
      'the pin holds the finished row for its beat',
    ).toHaveLength(0);
    vi.advanceTimersByTime(450);
    if (grow > 0) {
      // The row is laid out in the turn: the browser's count and the list's
      // measurement move together, which is what a real row's growth is.
      setModel(TOTAL + grow);
      element.height = TOTAL + grow;
    }
    // The page landing and the completion each pinned the old foot; what the
    // beat itself lands is the only pin the cases below are about.
    clear();
    flushSync();
    expect(
      document.querySelectorAll('details.turninfo'),
      'and the beat hands it back to the turn',
    ).toHaveLength(1);
  }

  it('lands the row the beat hands back, with no size report at all', async () => {
    // **The turn-end metadata below the fold.** The finished row moves out of
    // the pin and into the turn at the beat with no publish behind it, so the
    // beat itself is the signal - not a size notification.
    const server = stub();
    await running(server);
    readerAt(element.height - element.viewport);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      // The timer and the layout it causes land together: the row is in the
      // turn, the turn is taller, and nothing reports a size.
      await beat(server, ROW);

      const foot = Math.floor(TOTAL + ROW);
      expect(pinned(), 'the foot the beat handed back needs is pinned').toEqual([
        { asked: foot, landed: foot - VIEWPORT },
      ]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("lands the newest row's own growth when the column's size report is the one dropped", async () => {
    // **The row has its own observer, and that is the redundancy.** A turn
    // grown after its frame's publish measures later than the pass ran, and
    // the column's content observer is the notification a browser drops under
    // load - so the row's own report, on a different node, is what lands it.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    setModel(TOTAL + ROW);
    setElement(TOTAL + ROW, VIEWPORT);
    // Only the newest turn's own observer: every other node is skipped, the
    // column's watcher included, so the height reaches the key by that row
    // alone rather than by any row or by the content around them.
    resized(TOTAL + ROW, (node) => node.getAttribute('data-t') !== 't2');
    await settle();

    const foot = Math.floor(TOTAL + ROW);
    expect(pinned(), 'the foot the row moved is followed').toEqual([
      { asked: foot, landed: foot - VIEWPORT },
    ]);
  });

  it('lands the foot the composer took room from', async () => {
    // **The column's room is not the conversation's size.** The composer below
    // it grows on its own - a dock opening, a take's row appearing - and the
    // column's foot moves without one row of the conversation changing.
    const server = stub();
    await draw(server);
    readerAt(FOOT);
    await settle();
    clear();

    const roomier = VIEWPORT - 120;
    setElement(TOTAL, roomier);
    resized();
    await settle();

    expect(pinned(), 'the column re-lands on the foot its own room leaves').toEqual([
      { asked: ASKED, landed: ASKED - roomier },
    ]);
  });

  it('leaves a reader away from the foot alone through the beat and the room', async () => {
    // **The control**: both signals are heights, and a height is not a reason
    // to carry a reader who put themselves somewhere else.
    const server = stub();
    await running(server);
    readerAt(0);
    await settle();
    clear();

    vi.useFakeTimers();
    try {
      await beat(server, ROW);
      setElement(TOTAL + ROW, VIEWPORT - 120);
      resized();
      await vi.advanceTimersByTimeAsync(34);
      flushSync();

      expect(pinned(), 'a reader away from the foot stays where they left it').toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });
});

/**
 * **A document return is not a seat switch** (#2834).
 *
 * The document and the session are separate branches of the router, so opening
 * one unmounts the column and returning mounts a fresh one. The entry effect
 * re-arms the follow - #1673's rule for a seat switch - so a reader parked
 * mid-conversation is dropped to the foot on the same seat, the same
 * conversation, and a place they never left.
 *
 * The page unmounting is the router's half and the column is the one under
 * test: the return is this mount, over a conversation the connection kept.
 */
describe('a return to the seat the reader left', () => {
  /** How far a returned reader is put back: 40px rows and 10px into the second. */
  const PLACE = 50;

  /** The column, mounted, with a page on it and the reader parked mid-history. */
  async function parked(server: ReturnType<typeof stub>): Promise<void> {
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);
    readerAt(PLACE);
    await settle();
    expect(list(), 'the column is mounted with rows to hold').not.toBeNull();
    expect(element.offset, 'precondition: the reader is parked, not at the foot').toBe(PLACE);
  }

  /** The document opening: the route unmounts the column, and the browser
   *  throws the element holding the reader's offset away with it. */
  async function ontoDocument(): Promise<void> {
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();
  }

  /** The return: the same seat, a fresh column over the conversation kept. */
  function returned(server: ReturnType<typeof stub>): void {
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // The fresh list's rows are new elements, and jsdom lays nothing out.
    layOut(40);
  }

  it('puts a parked reader back at their place, and pins nothing', async () => {
    const server = stub();
    await parked(server);

    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    const foot = Math.floor(TOTAL) - VIEWPORT;
    expect(element.offset, 'the return dropped the reader to the foot').toBe(PLACE);
    // **The list moved to the row's turn before the place was held.** A fresh
    // list mounts at its start, so a row the reader left further down is
    // outside the drawn window until this scroll draws it; the stub draws
    // every row, so the call is the only half it can see.
    expect(moved(), 'the return did not scroll the list to the row it restores').toEqual([
      { at: 1, align: 'start' },
    ]);
    expect(
      pinned().filter((pin) => pin.landed === foot),
      'a return is not a seat switch: no foot pin',
    ).toEqual([]);
    expect(
      document.querySelector('button.follow'),
      'the returned reader is away from the foot, and the way back says so',
    ).not.toBeNull();
  });

  it('leaves a restore in flight alone when the layout moves the reader short', async () => {
    // **A restore in flight is not a foot for the pin to land.** The mount
    // re-arms the hold while `restoring` still names the row to place, and a
    // re-land there writes the foot with no anchor and forgets the place the
    // reader left, so a later return cannot take them back.
    const server = stub();
    await parked(server);
    await ontoDocument();
    returned(server);

    // The follow can be armed while the restore is still placing the reader.
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);

    // The layout moves toward the foot and lands short of it, before the
    // restore's own frame runs.
    const grown = Math.floor(TOTAL + 400);
    setElement(grown, VIEWPORT);
    list()?.scrolledTo(FOOT + 100, grown, VIEWPORT);
    flushSync();

    expect(
      pinned().filter((pin) => pin.landed === grown - VIEWPORT),
      'the restore in flight is not a pin to the foot',
    ).toEqual([]);

    // Let the follow go again so the restore can finish its own placement,
    // then come back: the place has to still be the reader's.
    chat?.following(false);
    await settle();
    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    expect(element.offset, 'the place survived the restore').toBe(PLACE);
  });

  /**
   * **The place is where the reader stopped, not where they first parked.**
   * Once the follow is off, another nudge changes nothing that publishes -
   * `following(false)` on an already-off flag is the same record - so a place
   * written from a publish would stay at the first nudge while the reader
   * keeps scrolling.
   */
  it('records where the reader stopped, not where they first parked', async () => {
    const server = stub();
    await parked(server);

    readerAt(30);
    await settle();

    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    expect(element.offset, 'the return went to the first place the reader left').toBe(30);
  });

  /**
   * **A send that lands the foot leaves no place behind.** The reader was
   * parked, then their own message took them to the latest; the pin is where
   * that move is made, and a place left standing would drag a later document
   * return back up to the row they left.
   */
  it('keeps no place for a reader their own send carried to the foot', async () => {
    const server = stub();
    await parked(server);

    server.prompt();
    await settle();
    expect(element.offset, 'precondition: the send landed the foot').toBe(FOOT);
    clear();

    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    expect(element.offset, 'the return restored the row the send left').toBe(FOOT);
  });

  /**
   * **The other direction: a place the reader gave up is not restored.** A
   * reader who scrolls back to the latest is done with their place, and the
   * foot branch that notices is what clears it; a later document return lands
   * at the latest rather than reopening the row.
   */
  it('forgets a place the reader gave up by returning to the latest', async () => {
    const server = stub();
    await parked(server);

    // An arrival arms the follow while the reader stays parked, so the trip to
    // the foot changes no published state and no pin follows it - the foot
    // branch's own clear is the only thing that can drop the place.
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();
    clear();

    readerAt(FOOT);
    await settle();
    clear();

    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    expect(element.offset, 'the return reopened a place the reader had left').toBe(FOOT);
  });

  /**
   * The membership guard's other case: a `/new` empties the conversation under
   * the place, so the row it names is no longer one this seat holds. The return
   * lands at the latest rather than scrolling to a row the new occupant never
   * wrote.
   */
  it('lands a /new at the foot on the next document return', async () => {
    const server = stub();
    await parkedWith(server, 's1');

    // `/new` on the seat: a different claude session.
    server.replaced('s2');
    await settle();
    clear();

    await ontoDocument();
    returned(server);
    // The new occupant's page reuses the fold's own turn keys, which is what
    // makes the row's name collide: without the entry's membership guard the
    // restore would find a turn to scroll to and take the place back.
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the return restored the old occupant place').toBe(
      Math.floor(TOTAL) - VIEWPORT,
    );
    expect(moved(), 'the emptied conversation has no row to scroll the list to').toEqual([]);
  });

  /**
   * **The follow flag is not the test** (#1673's own gate): an arrival re-arms
   * the follow, and the pin's condition holds the reader where they are rather
   * than carrying them down. That reader still holds a place, and a document
   * return owes it back - and the place is where they stopped, so a nudge after
   * the arm is recorded even though no publish follows it.
   */
  it('puts a parked reader back when an arrival armed the follow', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    // Enough rows to park below the disarm slack without reaching the foot.
    server.page(Array.from({ length: 30 }, (_, at) => spoken(`t${at}`)));
    await settle();
    layOut(40);

    // A first nudge parks the reader, and the follow turns off with it.
    readerAt(PLACE);
    await settle();
    expect(following(server), 'precondition: the reader parked, so the follow is off').toBe(false);

    // An arrival re-arms the follow while the reader stays parked.
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await settle();
    expect(following(server), 'precondition: the arrival armed the follow').toBe(true);

    clear();

    // The return takes the place back and parks the reader, whatever the
    // follow flag said when they left. (A place inside the 48px notch is not
    // a place - the notch follows - so this reader is parked clear of it.)
    await ontoDocument();
    returned(server);
    await settle();
    await settle();

    expect(element.offset, 'the return went to the place the reader left').toBe(PLACE);
    expect(following(server), 'the return turns the follow off under the parked reader').toBe(
      false,
    );
    expect(
      pinned().filter((pin) => pin.landed === Math.floor(TOTAL) - VIEWPORT),
      'a return is not a seat switch: no foot pin',
    ).toEqual([]);
  });

  /**
   * **A restore that cannot land falls back to the foot.** An arrival arms the
   * follow under the return, and the placer will not put a following reader
   * back mid-history - so rather than stranding them at the turn's start with
   * the follow off, the column re-arms the foot and lands there.
   */
  it('falls back to the foot when the follow is armed under a return', async () => {
    const server = stub();
    await parked(server);

    await ontoDocument();
    returned(server);
    // The restore's scroll runs on the first frame; the follow is armed
    // between it and the frame that would put the row back.
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    flushSync();
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    chat?.following(true);
    await new Promise((resolve) => requestAnimationFrame(() => resolve(null)));
    flushSync();
    await settle();

    expect(element.offset, 'the dropped restore left the reader at the turn start').toBe(FOOT);

    // And the return is finished with: the next one takes a fresh place, at an
    // offset the pre-fallback one does not hold.
    readerAt(30);
    await settle();
    await ontoDocument();
    returned(server);
    await settle();
    await settle();
    expect(element.offset, 'the dropped restore outlived its return').toBe(30);
  });

  /**
   * **A returning list has not measured when its first observer delivery
   * arrives.** Its estimate can fit the column while the drawn conversation
   * overflows, and a fit branch reading that estimate would re-arm the foot
   * and drop the recorded place under the restore - a short session with a few
   * long turns, exactly where a reader scrolls up, would land at the foot.
   */
  it('leaves a returning list to its restore until it has measured', async () => {
    const server = stub();
    await parked(server);

    await ontoDocument();
    // The return's list has not reported its viewport yet, and the element's
    // own box fits while the drawn rows overflow it.
    reported.viewport = 0;
    setElement(VIEWPORT, VIEWPORT);
    returned(server);
    // The observer's first delivery lands before the restore's own frame.
    resized();
    flushSync();
    // The real measure arrives, and the restore finishes.
    setMeasured(TOTAL, VIEWPORT);
    await settle();
    await settle();

    expect(element.offset, 'the unmeasured fit carried the return to the foot').toBe(PLACE);
  });

  /**
   * **And the same fit is refused on a list that HAS measured.** The restore's
   * placer owns the entry until it lands, whatever the list reports: a fit
   * that arrives between the mount and the placer's frame would otherwise
   * re-arm the foot over a reader the return is putting back.
   */
  it('does not re-arm the foot from a fit delivered under the restore', async () => {
    const server = stub();
    await parked(server);

    await ontoDocument();
    setElement(VIEWPORT, VIEWPORT);
    returned(server);
    resized();
    flushSync();
    setMeasured(TOTAL, VIEWPORT);
    await settle();
    await settle();

    expect(element.offset, 'the fit under the restore carried the reader to the foot').toBe(PLACE);
  });

  /**
   * **And a fit is no fit on a list that has not reported its viewport**,
   * after a return has landed: the estimate is not the drawn column, and
   * re-arming on it would carry a restored reader to the foot.
   */
  it('does not re-arm the foot from an unmeasured fit after the return', async () => {
    const server = stub();
    await parked(server);

    await ontoDocument();
    returned(server);
    await settle();
    await settle();
    expect(element.offset, 'precondition: the return put the reader back').toBe(PLACE);

    // The list stops reporting a viewport, and its element briefly fits.
    reported.viewport = 0;
    setElement(VIEWPORT, VIEWPORT);
    resized();
    flushSync();
    setMeasured(TOTAL, VIEWPORT);
    resized();
    await settle();

    expect(element.offset, 'the unmeasured fit reopened the foot').toBe(PLACE);
  });

  /**
   * **The restore's ladder is walked to its last rung.** A place whose turn is
   * still held but whose row is gone cannot be put back: the placer moves the
   * list to the turn once, waits its retries for the row, then gives up to the
   * foot. (The half where the row lands on a later frame needs a live list:
   * the stub draws every row it holds.)
   */
  it('gives up a place whose row is gone while its turn survives', async () => {
    const server = stub();
    await parked(server);

    // The place keeps a turn this conversation still holds, but names a row it
    // no longer draws - the re-key a fold can make in place.
    const which = subjectKey({ session: LEAD });
    const place = places.of(which);
    expect(place, 'precondition: the column recorded a place').toBeDefined();
    if (place !== undefined) {
      const turn = place.anchor.key.slice(0, place.anchor.key.indexOf(':'));
      places.keep(which, place.session, { key: `${turn}:gone`, into: place.anchor.into });
    }

    await ontoDocument();
    returned(server);
    await settle();
    await settle();
    await settle();

    expect(moved(), 'the placer moved the list to the surviving turn once').toEqual([
      { at: 1, align: 'start' },
    ]);
    expect(element.offset, 'the missing row ended the restore at the foot').toBe(FOOT);
  });

  /**
   * The guard on the other side: a seat the reader has never parked on opens
   * at the latest, and a seat that holds a place reopens it. The switch is
   * the same mounted column taking a different slot, which is what the app
   * does.
   */
  it('opens an unvisited seat at the foot and a parked one at its place', async () => {
    const server = stub();
    const seat = writable(LEAD);
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Seats, {
      target: document.body,
      props: { seat, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);
    readerAt(PLACE);
    await settle();
    clear();

    seat.set(OTHER);
    flushSync();
    server.page([spoken('o1'), spoken('o2')], OTHER);
    await settle();

    expect(element.offset, 'the switch did not open the new seat at its latest').toBe(
      Math.floor(TOTAL) - VIEWPORT,
    );

    // **And back to the seat that holds a place.** A seat switch is an entry
    // like any other now: the conversation was parked at `PLACE`, so the
    // switch back puts the reader there rather than at the foot. The rows
    // this seat's conversation draws are new elements, and jsdom lays nothing
    // out.
    seat.set(LEAD);
    flushSync();
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the switch back reopened the place the seat was left at').toBe(PLACE);
  });
});

/**
 * A conversation parked mid-history, over a column the test can then drive.
 *
 * Three turns with words, so the rows the anchor names exist, and a 40px row
 * the place arithmetic is read against.
 */
async function parkedColumn(server: ReturnType<typeof stub>): Promise<void> {
  clear();
  clearObservers();
  setMeasured(TOTAL, VIEWPORT);
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection },
  });
  flushSync();
  server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
  await settle();
  layOut(40);
  readerAt(50);
  await settle();
  expect(list(), 'the column is mounted with rows to hold').not.toBeNull();
  expect(element.offset, 'precondition: the reader is parked').toBe(50);
}

/** A column parked mid-history with its occupant's id already known. */
async function parkedWith(server: ReturnType<typeof stub>, session: string): Promise<void> {
  clear();
  clearObservers();
  setMeasured(TOTAL, VIEWPORT);
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection },
  });
  flushSync();
  server.session(session);
  server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
  await settle();
  layOut(40);
  readerAt(50);
  await settle();
  expect(element.offset, 'precondition: the reader is parked').toBe(50);
  expect(
    places.of(subjectKey({ session: LEAD }))?.session,
    'precondition: the place is tagged with the occupant',
  ).toBe(session);
}

/** The count drawn on the circle, or null while it draws none. */
function badge(): string | null {
  return document.querySelector('button.follow .n')?.textContent?.trim() ?? null;
}

/** The seat's follow flag, which the entry, a send and a notch return change. */
function following(server: ReturnType<typeof stub>): boolean | undefined {
  const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
  return chat === undefined ? undefined : get(chat.value).following;
}

/** The viewport offset of the row the reader's top edge is on. */
function topRowTop(): number | null {
  const root = document.querySelector('.conv');
  const live = root === null ? [] : [...root.querySelectorAll('.turn [data-k]')];
  for (const row of live) {
    const box = row.getBoundingClientRect();
    if (box.bottom > 0) return box.top;
  }
  return null;
}

describe('a saved place outliving the connection', () => {
  it('keeps the place a conversation was left at across a new connection', async () => {
    const first = stub();
    await parkedColumn(first);
    expect(
      places.of(subjectKey({ session: LEAD })),
      'the place is recorded while the reader is parked',
    ).toBeDefined();

    // The document goes, and the socket comes back as a NEW connection: the
    // conversation it held went with the old one, and so did every map the
    // connection owned. The production release step runs first, so the test
    // exercises the path a closed seat takes rather than only the unmount.
    if (app !== null) await unmount(app);
    app = null;
    releaseConversation(first.connection, subjectKey({ session: LEAD }));
    document.body.innerHTML = '';
    element.offset = 0;
    clear();

    const second = stub();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: second.connection },
    });
    flushSync();
    second.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the reconnect reopened the place the reader left').toBe(50);
  });

  it("releases a seat's conversation and keeps its place", async () => {
    const server = stub();
    await parkedColumn(server);
    const key = subjectKey({ session: LEAD });
    const held = conversationsFor(server.connection);
    const live = held.live.get(key);
    expect(live, 'precondition: the seat is live').toBeDefined();
    expect(held.kept.get(key), 'precondition: the conversation is kept').toBeDefined();
    expect(places.of(key), 'precondition: the place is recorded').toBeDefined();
    if (live === undefined) return;
    const before = get(live.chat.value);

    // **A second seat is the rest of the fleet.** The release must take only
    // the one seat's entries: a clear of the whole map reads identically on a
    // connection that held only this seat.
    const other = subjectKey({ session: OTHER });
    held.kept.set(other, get(live.chat.value));
    held.live.set(other, { connection: server.connection, chat: live.chat, stop: () => undefined });

    releaseConversation(server.connection, key);

    expect(held.live.get(key), "the released seat's entry is gone").toBeUndefined();
    expect(held.kept.get(key), 'the conversation went with the seat').toBeUndefined();
    expect(places.of(key), 'the place stays for a reconnect').toBeDefined();
    expect(held.kept.get(other), "another seat's conversation stays").toBeDefined();
    expect(held.live.get(other), 'and its open conversation too').toBeDefined();

    // **The listeners stop with it, which is what `stop` is for.** A frame on
    // the seat must not move the released conversation: without the call it
    // keeps folding a successor's frames into an object nothing draws.
    server.frame();
    await settle();
    expect(get(live.chat.value), 'the released conversation stopped folding the successor').toBe(
      before,
    );
  });

  it('opens a different claude session at the latest, whatever the slot was left holding', async () => {
    const server = stub();
    await parkedWith(server, 's1');

    // `/resume` to another claude session id: the same slot, a new
    // conversation. The place belonged to the occupant that left.
    server.replaced('another-session');
    await settle();
    expect(places.of(subjectKey({ session: LEAD })), 'the old place is gone').toBeUndefined();

    clear();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();

    expect(element.offset, 'the new session opened at the old place').toBe(
      Math.floor(TOTAL) - VIEWPORT,
    );
  });

  it('keeps the place through a re-spawn that resumes the same claude session', async () => {
    const server = stub();
    await parkedWith(server, 's1');

    // A replacement with the SAME claude session is the same conversation,
    // so the place is not the outgoing occupant's to lose.
    server.replaced('s1');
    await settle();
    expect(places.of(subjectKey({ session: LEAD })), 'the place survived its own').toBeDefined();

    // The successor's page, then a document return: the row is owed back.
    // **The element starts at the top**, or the offset the outgoing list left
    // would read as the place whether or not the reload restored one.
    element.offset = 0;
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();
    await settle();

    // **The reload places the reader on the seat, not only on a later
    // return.** The same-id replacement is the conversation reloading, so the
    // row it draws first must be the place the reader left.
    expect(element.offset, 'the re-spawn put the reader back on their row').toBe(50);

    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the return reopened the place it kept').toBe(50);
  });

  it('arms the restore when a same-id reload lacks the parked row', async () => {
    // **The reload's first page can start below the reader's row.** Arming the
    // restore on the replacement is what asks for the page above; without it
    // the column treats the row as gone and goes to the latest.
    const server = stub();
    await parkedWith(server, 's1');
    element.offset = 0;
    server.replaced('s1');
    await settle();
    clear();

    server.page([spoken('n1'), spoken('n2'), spoken('n3')], LEAD, 'cursor-above');
    layOut(40);
    await settle();
    await settle();

    expect(server.asks, 'the reload asked for the page that carries the parked row').toContain(
      'cursor-above',
    );
  });

  it('opens a same-id reload at the element foot when the model lags', async () => {
    // **The merge-holder.** A re-spawn that resumes the same claude session
    // reloads the record and mounts a new list, and the model has not
    // measured the rows already in the element. A first pin that holds at the
    // model's short foot lands the reader above the conversation and walks
    // them down as the measurement catches up - Ved's "starts somewhere up
    // and then scrolls down". The open must write the element's own foot.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.session('s1');
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    expect(element.offset, 'precondition: the reader is at the foot').toBe(FOOT);
    clear();

    // The element already counts the successor's rows; the list has not
    // measured them.
    setModel(TOTAL);
    setElement(TOTAL + 2000, VIEWPORT);
    server.replaced('s1');
    await settle();
    clear();

    server.page([spoken('n1'), spoken('n2'), spoken('n3')]);
    layOut(40);
    await settle();

    const grown = Math.floor(TOTAL + 2000);
    expect(pinned(), "the reload placed the reader at the element's foot").toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
    expect(element.offset, 'and the reader is there').toBe(grown - VIEWPORT);
  });

  it('keeps placing a same-id reload on the pass after it, while the model still lags', async () => {
    // **A remount re-opens the walk one pass later.** The reload places its
    // reader on the first pin; the pass after it is the settle's, and it must
    // not hold at the model's short foot. Clearing the settle only on a seat
    // switch is how this came back on `/resume` and on a re-spawn.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.session('s1');
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    expect(element.offset, 'precondition: the reader is at the foot').toBe(FOOT);
    clear();

    setModel(TOTAL);
    setElement(TOTAL + 2000, VIEWPORT);
    server.replaced('s1');
    await settle();
    clear();

    server.page([spoken('n1'), spoken('n2'), spoken('n3')]);
    layOut(40);
    await settle();
    clear();

    server.frame();
    await settle();

    const grown = Math.floor(TOTAL + 2000);
    expect(pinned(), 'the settle after the reload places the reader again').toEqual([
      { asked: grown, landed: grown - VIEWPORT },
    ]);
    expect(element.offset, 'and the reader never left it').toBe(grown - VIEWPORT);
  });

  it('restores the same row and offset after a live turn grows while the reader is away', async () => {
    // **The reported defect's shape.** The reader parks mid-history while the
    // newest turn is still being written, leaves the seat, and the turn keeps
    // growing under them. The return owes the row they stopped on, at the
    // offset they held - the growing turn is below it and may not move them.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);

    // A live turn opens and streams, so the newest row is being written.
    server.forged('stream');
    server.frame();
    await settle();
    layOut(40);

    // The reader parks on a page row, five pixels into it.
    readerAt(50);
    await settle();
    const which = subjectKey({ session: LEAD });
    const place = places.of(which);
    expect(place, 'the park recorded a place').toBeDefined();
    if (place === undefined) return;
    expect(place.anchor.into, 'the place is the row under the edge').toBe(10);
    clear();

    // The document goes; the live turn keeps growing while the reader is away.
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();
    server.frame();
    server.frame();
    server.frame();
    await settle();

    // The return puts them on the same row, at the same offset.
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the return moved the reader off their row').toBe(50);
    const row = document.querySelector(`[data-k="${CSS.escape(place.anchor.key)}"]`);
    expect(row, 'the row the place names is on screen').not.toBeNull();
    if (row !== null) {
      const box = document.querySelector('.conv')?.getBoundingClientRect();
      expect(
        Math.round(((row.getBoundingClientRect().top ?? 0) - (box?.top ?? 0)) * 100) / 100,
        'the row sits where the reader left it',
      ).toBe(-10);
    }
  });

  it('pages above a new connection until it reaches the parked row', async () => {
    // **A new connection's first page is the newest turns.** A reader parked
    // deeper than that page is not a reader whose row is gone: the place is
    // kept, the history above is asked for, and the page that carries the row
    // places them on it. Deleting the place here was the silent loss the book
    // now rules out.
    const first = stub();
    await parkedWith(first, 's1');
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();

    const second = stub();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: second.connection },
    });
    flushSync();
    // The newest page starts above the row the reader held, and says there is
    // more above it.
    second.session('s1');
    second.page([spoken('n1'), spoken('n2'), spoken('n3')], LEAD, 'cursor-above');
    layOut(40);
    await settle();
    await settle();

    expect(
      second.asks,
      'the restore asked for the page above instead of forgetting the place',
    ).toContain('cursor-above');
    expect(places.of(subjectKey({ session: LEAD })), 'and the place is still held').toBeDefined();

    // The page above carries the row, and the reader is placed on it.
    second.page([spoken('t1'), spoken('t2'), spoken('t3')], LEAD, null);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the page that carried the row placed the reader on it').toBe(50);
  });

  it('keeps the place when the restore budget runs out above the row', async () => {
    // **A page that never carries the row inside the budget.** The history
    // above is still loadable, so the place is not the conversation's to
    // forget: the reader goes to the latest, and the stored place stays for
    // the next return. The old entry forgot it through the landing's own pin.
    const first = stub();
    await parkedWith(first, 's1');
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();

    const second = stub();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: second.connection },
    });
    flushSync();
    second.session('s1');
    second.page([spoken('n1'), spoken('n2'), spoken('n3')], LEAD, 'cursor-above');
    layOut(40);
    await settle();
    await settle();

    // The budget is 1200ms; jump the clock past it and hand the restore a
    // page that still does not carry the row.
    const clock = vi.spyOn(Date, 'now').mockReturnValue(Date.now() + 2_000);
    try {
      second.page([spoken('n4'), spoken('n5')], LEAD, 'cursor-above');
      layOut(40);
      await settle();
      await settle();
    } finally {
      clock.mockRestore();
    }

    expect(
      places.of(subjectKey({ session: LEAD })),
      'the give-up kept the place for the next return',
    ).toBeDefined();
    expect(element.offset, 'and carried the reader to the latest').toBe(
      Math.floor(TOTAL) - VIEWPORT,
    );
  });

  it('keeps a place when a reconnect names the same claude session', async () => {
    const first = stub();
    await parkedWith(first, 's1');
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();

    const second = stub();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: second.connection },
    });
    flushSync();
    // The occupant first, as a subscribe answers, then the page.
    second.session('s1');
    second.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'the same occupant took its place back').toBe(50);
  });

  it('drops a place when a reconnect names a different claude session', async () => {
    const first = stub();
    await parkedWith(first, 's1');
    if (app !== null) await unmount(app);
    app = null;
    document.body.innerHTML = '';
    element.offset = 0;
    clear();

    const second = stub();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: second.connection },
    });
    flushSync();
    second.session('s2');
    second.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    layOut(40);
    await settle();
    await settle();

    expect(element.offset, 'a different occupant opened at the latest').toBe(
      Math.floor(TOTAL) - VIEWPORT,
    );
    expect(
      places.of(subjectKey({ session: LEAD })),
      'and the place went with the occupant that left',
    ).toBeUndefined();
  });

  it('lands a same-seat occupant change at once, not on a glide', async () => {
    // **A successor's first pin is a first pin.** The previous conversation's
    // foot is not this one's, so the new conversation lands at once rather
    // than sliding in from the old offset (the book's "a first pin, a
    // switch's or a return's, lands at once").
    const server = stub();
    await parkedWith(server, 's1');
    clear();

    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    server.replaced('another-session');
    await settle();
    clear();

    server.page([spoken('n1'), spoken('n2'), spoken('n3')]);
    layOut(40);
    await settle();

    expect(element.offset, 'the successor opened at the latest').toBe(FOOT);
    expect(pinned().length, 'the first pin was one write, not a glide').toBe(1);
  });
});

describe('the badge on the way back', () => {
  it('counts the turns appended since parking once each, and clears on the way back', async () => {
    const server = stub();
    await parkedColumn(server);
    expect(badge(), 'nothing has been appended yet').toBeNull();

    // A turn appended while the reader is parked.
    server.forged('cron');
    await settle();
    expect(badge(), 'the appended turn is counted').toBe('1');

    // A chunk joining the turn that opened is not a second turn.
    server.frame();
    await settle();
    expect(badge(), 'a streamed chunk is not a turn of its own').toBe('1');

    // A prompt opens the next turn only once the one before it settled.
    server.result();
    server.forged('peer');
    await settle();
    expect(badge(), 'the second turn is counted').toBe('2');

    // Nine or more reads as 9+, which is the cap the circle draws.
    for (const source of ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']) {
      server.result();
      server.forged(source);
      await settle();
    }
    expect(badge(), 'the count caps at nine or more').toBe('9+');

    const back = document.querySelector('button.follow');
    expect(back, 'precondition: the way back is drawn').not.toBeNull();
    back?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await settle();

    expect(badge(), 'following clears the badge').toBeNull();
    expect(element.offset, 'and the circle lands the foot').toBe(FOOT);

    // **A count that was cleared does not come back with the next park.**
    // The circle is gone at the foot, so reading its absence there proves
    // nothing; the park after it is what says the count itself was reset.
    readerAt(50);
    await settle();
    expect(badge(), 'the cleared count reopened with the next park').toBeNull();
  });

  it('counts a send the reader made on another client, and does not carry them to it', async () => {
    // **`source: 'you'` is every client of the reader, not this one.** The
    // press that carries a parked reader to the foot is the one made here,
    // under their own eye; the phone's message lands below them and rides the
    // badge like any other turn they were away for.
    const server = stub();
    await parkedColumn(server);
    expect(badge(), 'nothing appended yet').toBeNull();

    server.remotePrompt();
    await settle();

    expect(badge(), "the phone's send is counted").toBe('1');
    expect(element.offset, 'and the reader stays where they are').toBe(50);
  });

  it('counts the first turn after a switch into a seat left parked', async () => {
    // **The marker is the column's, not the seat's.** The arriving seat is
    // already parked and keeps its turns, so the badge's `!parked` clear
    // never runs on it: the switch's own run must arm its marker before the
    // first arrival, or that arrival is the run that arms it and the badge
    // never counts the turn that woke it.
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
    server.page([spoken('t1'), spoken('t2')]);
    await settle();
    layOut(40);

    // An earlier visit parks the other seat, so it is already parked when the
    // reader comes back to it.
    seat.set(OTHER);
    flushSync();
    server.page([spoken('o1'), spoken('o2')], OTHER);
    await settle();
    layOut(40);
    readerAt(0);
    await settle();

    // Back to the first seat, and park it: its marker is the one the next
    // switch has to drop.
    seat.set(LEAD);
    flushSync();
    await settle();
    layOut(40);
    readerAt(0);
    await settle();
    expect(badge(), 'nothing has been appended yet').toBeNull();

    // The switch into the parked seat, and one arrival.
    seat.set(OTHER);
    flushSync();
    server.forgedOn(OTHER);
    await settle();

    expect(badge(), 'the first turn after the switch is counted').toBe('1');
  });

  it('keeps a parked reader where they scrolled when a frame lands', async () => {
    // **The re-arm is the replacement's, not every publish's.** With an id
    // known, a run on any publish would take the viewport back to the place
    // the reader left: they scroll to 200 as a frame lands, and the return
    // placer drags them to 50.
    const server = stub();
    await parkedWith(server, 's1');
    clear();

    readerAt(70);
    server.frame();
    await settle();

    expect(element.offset, 'the reader keeps the row they scrolled to').toBe(70);
    expect(following(server), 'and the follow stays off').toBe(false);
  });

  it('carries the reader for a send whose echo it never held', async () => {
    // **The id is the send's own, wherever it left from.** A takeover answer,
    // a palette command and a review or document nudge carry no echo of their
    // own, and the echo the composer posts can be cleared by a word match
    // before its frame arrives. The minted id is what still says the words
    // are this client's, so a parked reader is carried to the foot instead of
    // counting their own send in the badge.
    const server = stub();
    // **An id is known here on purpose.** A same-id re-arm that ran on every
    // publish would switch the follow off and take the reader back to their
    // old place as this send lands, which is how the bug hid behind the
    // id-less parked fixtures.
    await parkedWith(server, 's1');
    readerAt(0);
    await settle();
    expect(following(server), 'precondition: the reader is parked').toBe(false);
    clear();

    echoes.remember(subjectKey({ session: LEAD }), 'pressed-here');
    server.ownFrame('pressed-here', 'what is this');
    await settle();

    expect(following(server), "the reader's own send carried them to the foot").toBe(true);
    expect(element.offset, 'and the column is there').toBe(FOOT);
  });
});

describe('the motion of a following column', () => {
  /**
   * A manual animation clock, so a glide is observed frame by frame rather
   * than waited out in real time. The column publishes on a frame as well,
   * so pumping is what runs the follow's own pass.
   */
  function clock(): { pump: (ms: number) => Promise<void>; stop: () => void } {
    let now = 0;
    const queue = new Map<number, FrameRequestCallback>();
    let next = 1;
    const realRaf = globalThis.requestAnimationFrame;
    const realCancel = globalThis.cancelAnimationFrame;
    const nowSpy = vi.spyOn(performance, 'now').mockImplementation(() => now);
    vi.stubGlobal('requestAnimationFrame', (fn: FrameRequestCallback) => {
      const id = next;
      next += 1;
      queue.set(id, fn);
      return id;
    });
    vi.stubGlobal('cancelAnimationFrame', (id: number) => {
      queue.delete(id);
    });
    return {
      pump: async (ms: number) => {
        now += ms;
        for (const [id, fn] of [...queue]) {
          queue.delete(id);
          fn(now);
        }
        flushSync();
        // **Let the write's scroll event run before the next frame.** A real
        // browser delivers it between frames and the column's guard reads it;
        // a pump that never yields would let a glide that dies on its first
        // clamp read as one that eased all the way.
        await new Promise((resolve) => setTimeout(resolve, 0));
      },
      stop: () => {
        nowSpy.mockRestore();
        vi.stubGlobal('requestAnimationFrame', realRaf);
        vi.stubGlobal('cancelAnimationFrame', realCancel);
      },
    };
  }

  it('glides toward the moving foot on an append and on a stream growth', async () => {
    // The landing that opens the column is the suite's instant one; the
    // motion starts with the append this case is about.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();
    layOut(40);
    expect(element.offset, 'precondition: the landing is at the foot').toBe(FOOT);
    clear();

    // No reduced-motion preference: this is the reader the glide is for.
    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    const hand = clock();
    try {
      // The turn appends and the column's own foot moves with it. The element
      // reports whole pixels, so that is the foot the landing reaches.
      const moved = Math.floor(TOTAL + 200) - VIEWPORT;
      reported.size = TOTAL + 200;
      setElement(TOTAL + 200, VIEWPORT);
      server.forged('one');
      await hand.pump(0);
      await hand.pump(16);
      const first = element.offset;
      expect(first, 'the column jumped to the foot instead of gliding').toBeLessThan(moved);
      expect(first, 'and it did move toward it').toBeGreaterThan(FOOT);
      // **The ease is spread across the glide, not spent in its first third.**
      // A tween aimed at the raw `scrollHeight` asks for a value past the
      // clamp from early on: the viewport arrives while most of the duration
      // is still to run and sits there. At 64ms of 160 the column must still
      // be travelling.
      for (let at = 0; at < 3; at += 1) await hand.pump(16);
      expect(element.offset, 'the ease reached the foot before its time').toBeLessThan(moved);
      for (let at = 0; at < 7; at += 1) await hand.pump(16);
      expect(element.offset, 'the glide lands the measured foot').toBe(moved);
      expect(
        pinned().length,
        'the glide wrote its frames rather than dying on the clamp',
      ).toBeGreaterThanOrEqual(6);

      // A row that grows under the finger during a stream is the same glide.
      clear();
      const grown = Math.floor(TOTAL + 400) - VIEWPORT;
      reported.size = TOTAL + 400;
      setElement(TOTAL + 400, VIEWPORT);
      resized(60);
      flushSync();
      await hand.pump(0);
      await hand.pump(16);
      expect(element.offset, 'the stream growth glided rather than jumped').toBeLessThan(grown);
      expect(element.offset, 'and it glided to the foot it measured').toBeGreaterThan(moved);
    } finally {
      hand.stop();
    }
  });

  it("keeps the follow when a layout move interrupts a send's glide", async () => {
    // **A correction half way through the landing is not the reader leaving.**
    // The send armed the follow and the column is gliding to the foot; a
    // hand-less layout move lands mid-travel. Stopping the glide there is
    // right, but reading the stop as the reader would strand them with the
    // follow switched off by a move they never made.
    const server = stub();
    await draw(server);
    layOut(40);
    readerAt(50);
    await settle();
    expect(element.offset, 'precondition: the reader is parked').toBe(50);

    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    const hand = clock();
    try {
      server.prompt();
      await hand.pump(0);
      await hand.pump(16);
      const mid = element.offset;
      expect(mid, 'precondition: the send is gliding to the foot').toBeGreaterThan(50);
      expect(mid, 'precondition: and it has not arrived').toBeLessThan(FOOT);

      // A layout move the glide did not write, with no hand behind it.
      list()?.scrolledTo(mid - 100, TOTAL, VIEWPORT);
      flushSync();
      expect(following(server), 'the layout move switched the follow off').toBe(true);

      for (let at = 0; at < 12; at += 1) await hand.pump(16);
      expect(element.offset, 'the column still carried the reader to the foot').toBe(FOOT);
    } finally {
      hand.stop();
    }
  });

  it('disarms when a small up-scroll stops a glide mid-stream', async () => {
    // **A gesture that stops a glide leaves the reader where it reached.**
    // `placed` is the offset the disarm compares against; a stale foot from
    // before the growth would let a small up-scroll inside the notch fail the
    // disarm, and the next growth would carry the reader back (#1673).
    const server = stub();
    await draw(server);
    layOut(40);
    expect(element.offset, 'precondition: the landing is at the foot').toBe(FOOT);
    clear();

    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    const hand = clock();
    try {
      // A stream growth starts a glide toward the new foot.
      const grown = Math.floor(TOTAL + 200) - VIEWPORT;
      reported.size = TOTAL + 200;
      setElement(TOTAL + 200, VIEWPORT);
      server.forged('one');
      await hand.pump(0);
      await hand.pump(16);
      const mid = element.offset;
      expect(mid, 'precondition: the glide is in flight').toBeGreaterThan(FOOT);
      expect(mid, 'precondition: and it has not arrived').toBeLessThan(grown);

      // The reader wheels up a little, stopping the glide inside the notch.
      document
        .querySelector('.conv')
        ?.dispatchEvent(new WheelEvent('wheel', { deltaY: -10, bubbles: true }));
      list()?.scrolledTo(mid - 10, TOTAL + 200, VIEWPORT);
      await hand.pump(16);

      expect(following(server), 'the small up-scroll disarmed the follow').toBe(false);

      // The next growth must not carry them back.
      reported.size = TOTAL + 400;
      setElement(TOTAL + 400, VIEWPORT);
      server.forged('two');
      for (let at = 0; at < 12; at += 1) await hand.pump(16);
      expect(element.offset, 'the next growth left the reader where they stopped').toBe(mid - 10);
    } finally {
      hand.stop();
    }
  });

  it('lands at once when the reader asked for less motion', async () => {
    // The suite's own preference is `reduce`, and the landing is one write.
    const server = stub();
    await draw(server);
    clear();

    reported.size = TOTAL + 200;
    setElement(TOTAL + 200, VIEWPORT);
    server.forged('one');
    await settle();

    expect(pinned().at(-1), 'the landing wrote the foot directly').toEqual({
      asked: Math.floor(TOTAL + 200),
      landed: Math.floor(TOTAL + 200) - VIEWPORT,
    });
    expect(element.offset, 'and the reader is there').toBe(Math.floor(TOTAL + 200) - VIEWPORT);
  });

  it('keeps the circle an instant jump', async () => {
    vi.stubGlobal('matchMedia', () => ({ matches: false }));
    const server = stub();
    await parkedColumn(server);
    clear();

    document
      .querySelector('button.follow')
      ?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    flushSync();

    expect(element.offset, 'the circle glided instead of jumping').toBe(FOOT);
  });
});

describe('the freeze a parked column keeps', () => {
  it('leaves the anchored row at the same sub-pixel offset through a reflow above it', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();

    // Fractional rows and a fractional offset: the drift a rounding anchor
    // throws away is exactly the half pixel a reader sees.
    layOut(40.5);
    readerAt(50.25);
    await settle();
    const before = topRowTop();
    expect(before, 'precondition: a row holds the top edge').not.toBeNull();
    clear();

    // A row above grows by three quarters of a pixel, and a turn appends
    // below it; neither may move the words under the reader's eye.
    layOut(41.25);
    resized();
    server.forged('below');
    await settle();

    expect(topRowTop(), 'the reflow moved the row under the reader').toBe(before);
  });

  it('keeps the same row at the top when the column is resized under a parked reader', async () => {
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1'), spoken('t2'), spoken('t3')]);
    await settle();

    layOut(40);
    readerAt(50);
    await settle();
    const before = topRowTop();
    clear();

    // The phone turns: a shorter viewport, and the rows reflow a little.
    setElement(TOTAL, 260);
    layOut(41);
    resized();
    await settle();

    expect(topRowTop(), 'the rotate moved the row under the reader').toBe(before);
    expect(element.offset, 'and the reader is still parked, not carried to the foot').toBe(51);
  });

  it('goes to the latest when a trim takes the row the reader holds', async () => {
    // **The plan's rule 7.** The held window drops whole pages over 8 MiB, and
    // a page dropped out from under a parked reader leaves their anchor naming
    // a row nothing holds. The pass cannot find it and would retry forever, so
    // the column goes to the one place that still exists rather than clamping
    // to whatever row is left.
    const server = stub();
    clear();
    clearObservers();
    setMeasured(TOTAL, VIEWPORT);
    app = mount(Chat, {
      target: document.body,
      props: { slot: LEAD, connection: server.connection },
    });
    flushSync();
    server.page([spoken('t1')], LEAD, 'cursor-seed');
    await settle();
    layOut(40);
    const chat = conversationsFor(server.connection).live.get(subjectKey({ session: LEAD }))?.chat;
    expect(chat, 'precondition: the column holds the seat conversation').toBeDefined();
    if (chat === undefined) return;
    expect(get(chat.value).loaded, 'precondition: the seed landed').toBe(true);
    expect(get(chat.value).cursor, 'precondition: the seed names the page above').toBe(
      'cursor-seed',
    );

    // Park first, so the page above lands without the follow's own trim. The
    // move near the top is the row's own ask for that page.
    readerAt(10);
    await settle();
    expect(server.asks.length, 'precondition: an older ask went out').toBeGreaterThanOrEqual(1);
    server.page([huge('huge', 9 << 20)], LEAD, 'cursor-older');
    await settle();
    // The rows the older page brought are new elements, and jsdom lays
    // nothing out; then the top edge moves into its huge turn.
    layOut(40);
    readerAt(0);
    await settle();
    expect(
      get(chat.value)
        .turns.map((t) => t.key)
        .includes('huge'),
      'the older page landed',
    ).toBe(true);
    expect(
      places.of(subjectKey({ session: LEAD }))?.anchor.key,
      'precondition: the reader holds a row of the older page',
    ).toMatch(/^huge:/);
    clear();

    // The trim the column runs when a reader returns to the latest.
    chat.trim(null);
    await settle();

    expect(following(server), 'the follow came back with the latest').toBe(true);
    expect(element.offset, 'and so did the reader').toBe(FOOT);
    expect(document.querySelector('button.follow'), 'the circle goes with the place').toBeNull();
  });
});

describe('the circle itself', () => {
  it('is a 44px square on every client, not the oval a raised height makes', () => {
    // The touch rules raise every button's MIN-HEIGHT to 44, which on its own
    // leaves a 34-wide control as an oval. The base rule carries both axes,
    // so the circle is a finger's target whatever raises the height.
    const sheet = readFileSync('src/assets/web.css', 'utf8');
    const body = /\.follow \{([^}]*)\}/.exec(sheet)?.[1] ?? '';
    expect(body, 'the follow button draws a width').toMatch(/width:\s*44px/);
    expect(body, 'the follow button draws a height').toMatch(/height:\s*44px/);
  });
});

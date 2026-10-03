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

const { clear, list, pinned, setElement, setMeasured } = await import('./testing/records');
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
    onStatus: () => () => undefined,
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
    page(turns: unknown[], seat: SessionSlot = LEAD): void {
      send({ kind: 'page', conversation: seat, turns, cursor: null });
    },
    /** One frame arriving on the seat, the way a running turn's do. */
    frame(text = 'a line arriving'): void {
      send({
        kind: 'update',
        update: { chat_appended: { key: LEAD, msg: said(text) } },
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

    expect(pinned(), 'the very end is the rule, not a threshold near it').toEqual([]);
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

    expect(pinned(), 'the end the reader is at is the element, not the model').toEqual([PIN, PIN]);
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

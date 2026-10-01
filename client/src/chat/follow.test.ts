// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
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

const { clear, list, pinned, setGeometry } = await import('./testing/records');
const { default: Chat } = await import('./Chat.svelte');

const { clearObservers, installResizeObserver, resized } = await import('./testing/viewport');

installResizeObserver();

const LEAD: SessionSlot = { org: 'Busytools', project: 'forge', label: 'lead' };

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
    page(turns: unknown[]): void {
      send({ kind: 'page', conversation: LEAD, turns, cursor: null });
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
  flushSync();
}

let app: Record<string, unknown> | null = null;

/** Where the reader is, which is what the follow is a decision about. */
function readerAt(offset: number): void {
  list()?.scrolledTo(offset, TOTAL, VIEWPORT);
  flushSync();
}

/** A clamp with no scroll event, which is what a resize can do to the reader. */
function clamped(at: number): void {
  list()?.settled(at, TOTAL, VIEWPORT);
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
  setGeometry(TOTAL, VIEWPORT);
  app = mount(Chat, {
    target: document.body,
    props: { slot: LEAD, connection: server.connection, cwd: null },
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

  it('re-arms when a size change leaves the reader at the very end', async () => {
    const server = stub();
    await draw(server);
    readerAt(0);
    await settle();
    clear();

    // A window grown until the history fits: the browser clamps the scroll to
    // the end of it and fires no scroll event, so the size change is the only
    // thing that can say the reader is now at the very end.
    clamped(FOOT);
    resized();
    await settle();
    clear();

    server.frame();
    await settle();

    expect(pinned(), 'the end they are sitting at is followed again').toEqual([PIN, PIN]);
  });
});

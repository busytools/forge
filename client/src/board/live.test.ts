// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { BoardRead } from '../wire/board';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { rereadBoard, watchBoard } from './live';

const BOARD: Subject = 'board';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** The same project's set with a task in it, which is news to the dedupe. */
const NEWS: ServerMessage = {
  kind: 'update',
  update: {
    tasks_changed: {
      key: LEAD,
      tasks: [
        { id: 't1', project_name: 'proj', subject: 'a row', status: 'pending' } as unknown as Task,
      ],
    },
  },
};

/** An update the board does not cover, and must do nothing for. */
const OTHER: ServerMessage = {
  kind: 'update',
  update: { chat_appended: { key: LEAD, msg: { type: 'assistant' } } },
};

/**
 * A connection a test drives by hand: what the board asked it to do, and a
 * way to hand it a frame. The stores are the real ones.
 */
function fakeConnection() {
  const stores = new Stores();
  const subscribed: Subject[] = [];
  const unsubscribed: Subject[] = [];
  const refreshed: Subject[] = [];
  const messages = new Set<(message: ServerMessage) => void>();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  let status: ConnectionStatus = 'open';

  const connection: Connection = {
    subscribe(what) {
      subscribed.push(what);
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
    holds: (what) => stores.get(what) !== undefined,
    refresh(what) {
      refreshed.push(what);
      return true;
    },
    showing() {},
    relist() {},
    declare() {},
    release() {},
    browserInput: () => false,
    onBrowserFrame: () => () => {},
    onMessage(fn) {
      messages.add(fn);
      return () => {
        messages.delete(fn);
      };
    },
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    dispatch: () => Promise.resolve(null),
    more: () => false,
    commitDiff: () => false,
    fileContent: () => false,
    readDocument: () => false,
    seatRange: () => false,
    frame: () => false,
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => status,
    lastHeard: () => null,
    onHeard: () => () => {},
    probe: () => {},
    close: () => {},
  };

  return {
    connection,
    stores,
    subscribed,
    unsubscribed,
    refreshed,
    listening: () => messages.size + statuses.size,
    arrive(message: ServerMessage) {
      for (const fn of [...messages]) fn(message);
    },
    setStatus(next: ConnectionStatus) {
      status = next;
      for (const fn of [...statuses]) fn(next);
    },
  };
}

/** A board record the server would answer a subscribe with. */
function boardWire() {
  return {
    projects: [
      {
        project: { org: 'TestOrg', name: 'proj' },
        rows: [],
        crons: [],
        connectors: { gotify: [], slack: [] },
      },
    ],
  };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the board over a connection', () => {
  /**
   * **A return asks one read at a time, and asks again once the answer lands
   * - even when the answer found nothing moved.** Latching the guard on a
   * version change would silence every return after one that found the board
   * unchanged.
   */
  it('asks a return read once, and again after an unchanged answer', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.refreshed.length = 0;

    rereadBoard(forge.connection);
    rereadBoard(forge.connection);
    expect(forge.refreshed, 'a second return queued behind the first').toEqual([BOARD]);

    // The answer lands with the same record and the same version: the guard
    // must still release on the write.
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    rereadBoard(forge.connection);
    expect(forge.refreshed, 'an unchanged answer latched the guard').toEqual([BOARD, BOARD]);
    stop();
  });

  it('subscribes once, shared by every reader of the board store', () => {
    const forge = fakeConnection();
    const first = watchBoard(forge.connection);
    const second = watchBoard(forge.connection);
    expect(first).toBe(second);
    const stop = first.subscribe(() => {});
    expect(forge.subscribed).toEqual([BOARD]);
    stop();
    expect(forge.unsubscribed, 'the last reader did not release the board').toEqual([BOARD]);
    expect(forge.listening(), 'listeners outlived the last reader').toBe(0);
  });

  it('draws the whole record the subscribe answer carried', () => {
    const forge = fakeConnection();
    const seen: unknown[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next.wire));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    expect(seen.at(-1)).toMatchObject({ projects: [{ project: { name: 'proj' } }] });
    stop();
  });

  it('applies a task write into the drawn board without a read', () => {
    const forge = fakeConnection();
    const seen: unknown[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next.wire));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive(NEWS);
    const rows = (seen.at(-1) as { projects: { rows: { task: { id: string } }[] }[] }).projects[0]
      ?.rows;
    expect(rows?.map((row) => row.task.id)).toEqual(['t1']);
    expect(forge.refreshed, 'a task write asked for a whole board read').toEqual([]);
    stop();
  });

  it('applies a schedule set without a read', () => {
    const forge = fakeConnection();
    const seen: unknown[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next.wire));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive({
      kind: 'update',
      update: {
        cron_schedules_changed: {
          key: LEAD,
          crons: [{ id: 'c1', schedule: '0 9 * * *', prompt: 'morning' }],
        },
      },
    } as unknown as ServerMessage);
    const crons = (seen.at(-1) as { projects: { crons: unknown[] }[] }).projects[0]?.crons;
    expect(crons).toHaveLength(1);
    expect(forge.refreshed).toEqual([]);
    stop();
  });

  it('applies a connector set without a read', () => {
    const forge = fakeConnection();
    const seen: unknown[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next.wire));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive({
      kind: 'update',
      update: {
        connector_subscriptions_changed: { key: LEAD, gotify: [], slack: [] },
      },
    });
    expect(forge.refreshed).toEqual([]);
    expect(seen.length).toBeGreaterThan(0);
    stop();
  });

  /**
   * **A drawer's own ask is applied where it lands.** The frame carries one
   * row whole - its brief and its history - so the drawer opens from the
   * frame rather than from a whole board read (rule 32).
   */
  it('applies a task_detail frame into the drawn board without a read', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive({
      kind: 'update',
      update: {
        task_detail: {
          key: LEAD,
          project: 'proj',
          row: {
            task: {
              id: 't-detail',
              project_name: 'proj',
              subject: 'a row',
              status: 'pending',
              detail: 'the brief',
            },
            history: [],
            history_total: 0,
          },
        },
      },
    } as unknown as ServerMessage);
    const rows = (
      seen.at(-1)?.wire as { projects: { rows: { task: { id: string; detail: string } }[] }[] }
    ).projects[0]?.rows;
    expect(rows?.map((row) => row.task.id)).toEqual(['t-detail']);
    expect(rows?.[0]?.task.detail, "the drawer's brief did not land").toBe('the brief');
    expect(forge.refreshed, "a drawer's ask bought a whole board read").toEqual([]);
    stop();
  });

  it('asks for nothing on a seat-liveness variant', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive({ kind: 'update', update: { releasing: { key: LEAD } } });
    expect(forge.refreshed, 'a seat event bought a whole board read').toEqual([]);
    stop();
  });

  it('ignores an update the board does not cover', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.arrive(OTHER);
    expect(forge.refreshed).toEqual([]);
    stop();
  });

  it('skips a task set that has not moved', () => {
    const forge = fakeConnection();
    let draws = 0;
    const stop = watchBoard(forge.connection).subscribe(() => {
      draws += 1;
    });
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    const before = draws;
    forge.arrive(NEWS);
    const afterFirst = draws;
    forge.arrive(NEWS);
    expect(afterFirst, 'the moved set did not redraw').toBeGreaterThan(before);
    expect(draws, 'the repeated set redrew again').toBe(afterFirst);
    stop();
  });

  /**
   * **A held patch is drawn, and a gap does not move the clock.** The gap
   * changes the store's state without a new record, so the instant the
   * drawn record landed must stay the one its worked time is measured from.
   */
  it('draws a held patch and keeps the clock on the record', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next));
    const store = forge.stores.get(BOARD);
    store?.set(boardWire(), 1);
    const landed = seen.at(-1)?.readAt ?? 0;
    expect(landed, 'the record landed with no instant').toBeGreaterThan(0);

    store?.gap('a skipped version');
    expect(seen.at(-1)?.resyncing, 'the gap is not drawn').toBe(true);
    expect(seen.at(-1)?.readAt, 'the gap reset the record clock').toBe(landed);

    store?.unplaced({ projects: 'unreadable' });
    expect(seen.at(-1)?.heldPatches, 'the held patch was not drawn').toHaveLength(1);
    expect(seen.at(-1)?.readAt, 'a held patch reset the record clock').toBe(landed);

    store?.applied(2, boardWire());
    expect(seen.at(-1)?.readAt ?? 0, 'a new record did not move the clock').toBeGreaterThanOrEqual(
      landed,
    );
    stop();
  });

  /**
   * **A frame fold writes the view whole, so it must carry what it did not
   * touch.** The held patches and the record clock live beside the wire, and
   * a fold that named only the wire dropped both: the patch vanished from the
   * page, and every other project's worked time re-anchored to this frame.
   */
  it('carries the held patches and the record clock through a frame fold', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next));
    const store = forge.stores.get(BOARD);

    vi.setSystemTime(1_000_000);
    store?.set(boardWire(), 1);
    const landed = seen.at(-1)?.readAt ?? 0;
    expect(landed, 'the record did not land at the read instant').toBe(1_000_000);

    vi.setSystemTime(1_060_000);
    store?.gap('a skipped version');
    store?.unplaced({ tasks_changed: 'unreadable' });
    forge.arrive(NEWS);

    expect(seen.at(-1)?.heldPatches, 'a frame fold dropped the held patch').toHaveLength(1);
    expect(seen.at(-1)?.resyncing, 'a frame fold dropped the catching-up state').toBe(true);
    expect(seen.at(-1)?.readAt, 'a frame fold reset the record clock').toBe(landed);
    const rows = (
      seen.at(-1)?.wire as
        { projects: { rows: { task: { id: string }; read_at?: number }[] }[] } | undefined
    )?.projects[0]?.rows;
    expect(
      rows?.[0]?.read_at,
      'the row the frame introduced is not anchored to its own frame',
    ).toBe(1_060_000);
    stop();
  });

  it('draws a refusal as the refusal, not an empty board', () => {
    const forge = fakeConnection();
    const seen: { refused: string | null }[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next));
    forge.stores.get(BOARD)?.refuse('forge holds no board for this project');
    expect(seen.at(-1)?.refused).toBe('forge holds no board for this project');
    stop();
  });

  it('redraws from a patch the socket applied, with the instant it was received', () => {
    const forge = fakeConnection();
    const seen: { wire: unknown; readAt?: number }[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next));
    const store = forge.stores.get(BOARD);
    store?.set(boardWire(), 1);
    const at = Date.now();
    store?.applied(2, {
      projects: [
        {
          project: { org: 'TestOrg', name: 'proj' },
          rows: [{ task: { id: 't-patch', project_name: 'proj', status: 'pending' } }],
          crons: [],
          connectors: { gotify: [], slack: [] },
        },
      ],
    });
    const rows = (seen.at(-1)?.wire as { projects: { rows: { task: { id: string } }[] }[] })
      .projects[0]?.rows;
    expect(rows?.map((row) => row.task.id)).toEqual(['t-patch']);
    expect(
      seen.at(-1)?.readAt,
      'the draw did not record when the record landed',
    ).toBeGreaterThanOrEqual(at);
    expect(forge.refreshed, 'a patch asked for a read').toEqual([]);
    stop();
  });

  /**
   * **No beat, no read, no timer.** Half an hour of a watched board whose
   * record does not move is thirty minutes of nothing: no refresh, no redraw,
   * and not one armed timer behind the page. This is the server's 300-second
   * mark re-subscribe, deleted.
   */
  it('sends nothing and arms nothing across half an hour of an unchanged board', async () => {
    const forge = fakeConnection();
    let draws = 0;
    const stop = watchBoard(forge.connection).subscribe(() => {
      draws += 1;
    });
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    const drawn = draws;
    await vi.advanceTimersByTimeAsync(30 * 60_000);
    expect(forge.refreshed, 'the board re-read itself').toEqual([]);
    expect(draws, 'the board redrew without news').toBe(drawn);
    expect(vi.getTimerCount(), 'a timer was left armed behind the page').toBe(0);
    stop();
  });

  it('releases the record when the connection drops, and redraws on the reconnect answer', () => {
    const forge = fakeConnection();
    const seen: unknown[] = [];
    const stop = watchBoard(forge.connection).subscribe((next) => seen.push(next.wire));
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    forge.setStatus('connecting');
    forge.stores.get(BOARD)?.set(boardWire(), 2);
    expect(seen.at(-1)).toMatchObject({ projects: [{ project: { name: 'proj' } }] });
    stop();
  });

  it('does not answer a refusal another subject owns', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.set(boardWire(), 1);
    // The socket attributes a refusal to its own store; another subject's
    // error reaching this page's listener changes nothing here.
    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no', seat: LEAD });
    expect(forge.refreshed).toEqual([]);
    stop();
  });
});

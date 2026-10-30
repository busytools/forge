import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { BoardRead } from '../wire/board';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { COALESCE_MS, watchBoard } from './live';

const BOARD: Subject = 'board';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/** One update a board subscriber is sent, which is what it answers with a read. */
const UPDATE: ServerMessage = {
  kind: 'update',
  update: { tasks_changed: { key: LEAD, tasks: [] } },
};

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

/** An update the board does not cover, and must not re-read for. */
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

  const connection: Connection = {
    subscribe(what) {
      subscribed.push(what);
      return stores.open(what);
    },
    unsubscribe(what) {
      unsubscribed.push(what);
      return stores.close(what);
    },
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
    status: () => 'open',
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
  };
}

/**
 * Drive the coalescing beat, which is the only clock these cases wait on.
 *
 * The beat is the store's own (`COALESCE_MS`), so it fires exactly when the
 * store would fire it, and no wall clock is left for a loaded runner to
 * outrun.
 */
async function settle(): Promise<void> {
  await vi.advanceTimersByTimeAsync(COALESCE_MS);
}

// Every case runs on fake timers, and the slow-beat cases below advance the
// same clock rather than installing one of their own.
beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the board over a connection', () => {
  it('subscribes once, shared by every reader of the board store', () => {
    const forge = fakeConnection();
    const first = watchBoard(forge.connection).subscribe(() => {});
    const second = watchBoard(forge.connection).subscribe(() => {});
    expect(forge.subscribed, 'the board was not subscribed once').toEqual([BOARD]);

    first();
    second();
    expect(forge.unsubscribed, 'the subscription did not go with the last reader').toEqual([BOARD]);
    expect(forge.listening(), 'a listener outlived the last reader').toBe(0);
  });

  it('answers with the whole read the server sent', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe(($next) => {
      seen.push($next);
    });

    const data = { projects: [] };
    forge.stores.get(BOARD)?.set(data);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data });
    expect(seen.at(-1)?.wire, 'the board snapshot did not reach the store').toEqual(data);
    expect(seen.at(-1)?.refused).toBeNull();
    stop();
  });

  it('asks again for a board update, and not for one the board does not carry', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a task write did not ask for a board read').toEqual([BOARD]);

    forge.arrive(OTHER);
    await settle();
    expect(forge.refreshed, 'a chat frame asked for a board read').toEqual([BOARD]);
    stop();
  });

  it('re-reads on the slow beat while the board is mounted', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    expect(forge.refreshed).toEqual([]);

    vi.advanceTimersByTime(29_000);
    expect(forge.refreshed, 'the beat fired early').toEqual([]);
    vi.advanceTimersByTime(1_000);
    expect(forge.refreshed, "the beat did not advance the rows' clocks").toEqual([BOARD]);

    stop();
    vi.advanceTimersByTime(60_000);
    expect(forge.refreshed, 'the beat outlived the last reader').toEqual([BOARD]);
  });

  it('coalesces a burst into one read', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    forge.arrive(UPDATE);
    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'a burst was not coalesced').toEqual([BOARD]);
    stop();
  });

  /**
   * **A re-announced task set is no read** (#2540). The chase used to send
   * every project every 30 s with the set unchanged, and answering that with
   * a full 628 KB board read was the loop the installed client was paying;
   * only a moved set asks.
   */
  it('skips a task set that has not moved', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    const task = {
      id: 't1',
      project_name: 'proj',
      subject: 'a row',
      status: 'pending',
    } as unknown as Task;
    const same: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [task] } },
    };
    const moved: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [{ ...task, status: 'completed' }] } },
    };

    forge.arrive(same);
    await settle();
    expect(forge.refreshed, 'the first task set asked for a read').toEqual([BOARD]);

    // The read in hand answers, so a later moved set can ask again.
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    await settle();

    // A fresh deep-equal object, not the same reference: the dedupe must
    // read the set, not the object identity.
    forge.arrive({
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [{ ...task }] } },
    });
    await settle();
    expect(forge.refreshed, 'an unchanged task set asked for another read').toEqual([BOARD]);

    forge.arrive(moved);
    await settle();
    expect(forge.refreshed, 'a moved task set did not ask for a read').toEqual([BOARD, BOARD]);
    stop();
  });

  /**
   * An empty project is the case the first dedupe missed: an update can
   * carry `[]` for a project with no tasks, so an `[]` that is `[]` again
   * must not ask, while an `[]` after a set is a deletion and must.
   */
  it('skips an unchanged empty set and reads a deletion', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    const empty: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [] } },
    };
    const one: ServerMessage = {
      kind: 'update',
      update: {
        tasks_changed: {
          key: LEAD,
          tasks: [
            {
              id: 't1',
              project_name: 'proj',
              subject: 'a row',
              status: 'pending',
            } as unknown as Task,
          ],
        },
      },
    };

    forge.arrive(empty);
    await settle();
    expect(forge.refreshed, 'the first empty set asked for a read').toEqual([BOARD]);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    await settle();

    forge.arrive(empty);
    await settle();
    expect(forge.refreshed, 'an unchanged empty set asked again').toEqual([BOARD]);

    forge.arrive(one);
    await settle();
    expect(forge.refreshed, 'a task added did not ask for a read').toEqual([BOARD, BOARD]);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    await settle();

    forge.arrive(empty);
    await settle();
    expect(forge.refreshed, 'a deletion did not ask for a read').toEqual([BOARD, BOARD, BOARD]);
    stop();
  });

  it('asks again when an update lands while a read is in flight', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed).toEqual([BOARD]);
    // The answer is in flight and a NEWS update lands behind it: the read in
    // hand was encoded before it, so the update is answered as soon as it
    // lands.
    forge.arrive(NEWS);
    forge.stores.get(BOARD)?.set({ projects: [] });
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    await settle();
    expect(forge.refreshed, 'the update that crossed the read was dropped').toEqual([BOARD, BOARD]);
    stop();
  });

  it('ignores a refusal another subject owns', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe(($next) => seen.push($next));
    forge.stores.get(BOARD)?.set({ projects: [] });
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    const before = seen.length;

    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'the home was refused',
      seat: null,
      subject: 'home',
    });
    expect(seen.length, "another subject's refusal re-set the board").toBe(before);
    stop();
  });

  it('does not ask again on the beat once the board is refused', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.refuse('this forge holds no board');
    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no board',
      seat: null,
      subject: BOARD,
    });

    vi.advanceTimersByTime(30_000);
    expect(forge.refreshed, 'a refused board was asked again on the beat').toEqual([]);
    stop();
  });

  it('does not ask again for an update once the board is refused', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    forge.stores.get(BOARD)?.refuse('this forge holds no board');
    forge.arrive({
      kind: 'error',
      what: 'subscribe',
      why: 'this forge holds no board',
      seat: null,
      subject: BOARD,
    });
    expect(forge.refreshed).toEqual([]);

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'an update asked a refused board again').toEqual([]);
    stop();
  });

  it('does not double-ask when the beat lands inside the coalesce window', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    // Just before the beat, an update schedules its read 50 ms out; the
    // beat fires first and the timer behind it must not ask again.
    vi.advanceTimersByTime(29_990);
    forge.arrive(UPDATE);
    vi.advanceTimersByTime(10);
    vi.advanceTimersByTime(40);
    expect(forge.refreshed, 'the beat and the coalesce timer both asked').toEqual([BOARD]);
    stop();
  });

  it('reads a refusal as the board refusal rather than an empty board', () => {
    const forge = fakeConnection();
    const seen: BoardRead[] = [];
    const stop = watchBoard(forge.connection).subscribe(($next) => {
      seen.push($next);
    });

    forge.stores.get(BOARD)?.refuse('no such subject');
    forge.arrive({ kind: 'error', what: 'subscribe', why: 'no such subject' });
    expect(seen.at(-1)?.refused, 'the refusal did not reach the store').toBe('no such subject');
    expect(seen.at(-1)?.wire).toBeNull();
    stop();
  });
});

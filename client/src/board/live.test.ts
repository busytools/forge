// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ServerMessage, Subject } from '../protocol';
import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { BoardRead } from '../wire/board';
import type { Task } from '../wire/home';
import type { SessionSlot } from '../wire/types';
import { COALESCE_MS, rereadBoard, watchBoard } from './live';

/**
 * The shell's resume listener, captured rather than registered: the event the
 * Rust half emits is the phone's only return signal, and a test drives it from
 * here. Each install keeps its event name beside its unlisten, so a release
 * can be asserted on the arm it belongs to.
 */
const { foreground, installed } = vi.hoisted(() => ({
  foreground: [] as ((event: unknown) => void)[],
  installed: [] as { name: string; stop: ReturnType<typeof vi.fn> }[],
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, cb: (event: unknown) => void) => {
    if (name === 'client://foreground') foreground.push(cb);
    const stop = vi.fn();
    installed.push({ name, stop });
    return Promise.resolve(stop);
  },
}));

/** The unlisten a listener's install was given, found by the event it serves. */
function stopFor(name: string): ReturnType<typeof vi.fn> | undefined {
  return installed.find((entry) => entry.name === name)?.stop;
}

const BOARD: Subject = 'board';
const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * How often a watched board re-reads while nothing writes.
 *
 * Spelled out rather than imported, so the tests state the cadence the
 * product promises rather than reading it back off the code they check.
 */
const MARK_BEAT_MS = 5 * 60_000;

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

/**
 * Whether the window has focus, and whether the document is hidden, as the
 * tests drive them.
 *
 * **Neither signal is jsdom's to decide**: `hasFocus()` is false with nothing
 * focused, and `hidden` is the environment's - so a test states the page it is
 * in and then fires the event a browser would. They are two flags because
 * they are two signals: a locked screen is a lost focus, a background tab is a
 * hidden document, and each must decide its case alone.
 */
let focused = true;
let hidden = false;

// Every case runs on fake timers, and the slow-beat cases below advance the
// same clock rather than installing one of their own.
beforeEach(() => {
  vi.useFakeTimers();
  focused = true;
  hidden = false;
  foreground.length = 0;
  installed.length = 0;
  // The shell marker is per-case: one leaked from a case that forgot to clean
  // up would arm the foreground listener under every later case.
  expect('__TAURI_INTERNALS__' in window, 'a shell marker outlived its case').toBe(false);
  vi.spyOn(document, 'hasFocus').mockImplementation(() => focused);
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
});

afterEach(() => {
  vi.restoreAllMocks();
  delete (document as unknown as { hidden?: boolean }).hidden;
  delete (window as unknown as Record<string, unknown>)['__TAURI_INTERNALS__'];
  vi.useRealTimers();
});

describe('the board over a connection', () => {
  /**
   * **A return asks through the page's guards.** One read at a time: an ask
   * while one is in flight would queue a second whole encode behind it, and a
   * subject nobody watches is not asked for at all.
   */
  it('a return reads through the page, one at a time', () => {
    const fake = fakeConnection();
    const stop = watchBoard(fake.connection).subscribe(() => {});

    rereadBoard(fake.connection);
    expect(fake.refreshed, 'the return asks for the board').toEqual([BOARD]);
    rereadBoard(fake.connection);
    expect(fake.refreshed, 'a read in flight is not asked again').toEqual([BOARD]);

    fake.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    rereadBoard(fake.connection);
    expect(fake.refreshed, 'the answered read frees the next').toEqual([BOARD, BOARD]);

    stop();
    fake.refreshed.length = 0;
    rereadBoard(fake.connection);
    expect(fake.refreshed, 'an unwatched board is not asked for').toEqual([]);
  });

  it('subscribes once, shared by every reader of the board store', () => {
    const forge = fakeConnection();
    const removedWindow = vi.spyOn(window, 'removeEventListener');
    const removedDocument = vi.spyOn(document, 'removeEventListener');
    const first = watchBoard(forge.connection).subscribe(() => {});
    const second = watchBoard(forge.connection).subscribe(() => {});
    expect(forge.subscribed, 'the board was not subscribed once').toEqual([BOARD]);

    first();
    second();
    expect(forge.unsubscribed, 'the subscription did not go with the last reader').toEqual([BOARD]);
    expect(forge.listening(), 'a listener outlived the last reader').toBe(0);
    // Every listener the attention observer installed is taken back.
    expect(
      removedDocument.mock.calls.map(([type]) => type),
      'the document listener outlived the last reader',
    ).toContain('visibilitychange');
    expect(
      removedWindow.mock.calls.map(([type]) => type),
      'a window listener outlived the last reader',
    ).toEqual(expect.arrayContaining(['focus', 'blur', 'pageshow']));
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

  it('re-reads on the marks beat while the board is watched', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    expect(forge.refreshed).toEqual([]);

    vi.advanceTimersByTime(MARK_BEAT_MS - 1);
    expect(forge.refreshed, 'the beat fired early').toEqual([]);
    vi.advanceTimersByTime(1);
    expect(forge.refreshed, "the beat did not advance the rows' marks").toEqual([BOARD]);

    // The beat recurs, so a one-shot timer cannot pass: the first answer
    // lands and the next cadence buys another read.
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat did not fire a second time').toEqual([BOARD, BOARD]);

    stop();
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat outlived the last reader').toEqual([BOARD, BOARD]);
  });

  /**
   * **The beat is one of the client's largest reads and runs only for a
   * reader.** A locked screen or a display going off reports `blur`, and a
   * background tab reports `visibilitychange`; both leave nobody to read an
   * answer.
   */
  it('stops the beat when the window loses attention', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    focused = false;
    window.dispatchEvent(new Event('blur'));
    vi.advanceTimersByTime(MARK_BEAT_MS * 3);
    expect(forge.refreshed, 'an unwatched page was read on the beat').toEqual([]);
    stop();
  });

  it('stops the beat when the document goes hidden, with focus still held', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    // Focus stays true, so `document.hidden` is the only signal that can
    // stop the beat here.
    hidden = true;
    document.dispatchEvent(new Event('visibilitychange'));
    vi.advanceTimersByTime(MARK_BEAT_MS * 3);
    expect(forge.refreshed, 'a hidden page was read on the beat').toEqual([]);
    stop();
  });

  it('restarts the beat on the return, without a read of its own', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    focused = false;
    window.dispatchEvent(new Event('blur'));
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'an unwatched page was read on the beat').toEqual([]);

    focused = true;
    window.dispatchEvent(new Event('focus'));
    // **The coalesce window is flushed**: a gain that only scheduled a read
    // would land inside it, and a synchronous assertion would miss it.
    await settle();
    expect(forge.refreshed, "the return asked for the resync read's job").toEqual([]);

    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat did not restart with the attention').toEqual([BOARD]);
    stop();
  });

  it('asks nothing for an attention report that changes nothing', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    // The page was already watched when the board was taken, so a report of
    // the same state - a restored page, a repeated focus - is no return.
    window.dispatchEvent(new Event('focus'));
    await settle();
    expect(forge.refreshed, 'a repeated attention report bought a read').toEqual([]);
    stop();
  });

  it('starts no beat for a board watched while the page is not', async () => {
    focused = false;
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    vi.advanceTimersByTime(MARK_BEAT_MS * 2);
    expect(forge.refreshed, 'a board nobody watches was read on the beat').toEqual([]);

    focused = true;
    window.dispatchEvent(new Event('focus'));
    await settle();
    expect(forge.refreshed, 'the return asked for a read of its own').toEqual([]);
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat did not start with the attention').toEqual([BOARD]);
    stop();
  });

  /**
   * **One interval, however many reports of the same gain arrive.** A second
   * `setInterval` would orphan the first, and the orphan keeps firing after
   * the page hides - the polling this change removes.
   */
  it('keeps one beat across a repeated attention report', () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    window.dispatchEvent(new Event('focus'));
    focused = false;
    window.dispatchEvent(new Event('blur'));
    vi.advanceTimersByTime(MARK_BEAT_MS * 3);
    expect(forge.refreshed, 'an orphaned beat read a page nobody watches').toEqual([]);
    stop();
  });

  it('does not double-ask when attention returns while a read is in flight', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the update did not ask for its read').toEqual([BOARD]);

    focused = false;
    window.dispatchEvent(new Event('blur'));
    focused = true;
    window.dispatchEvent(new Event('focus'));
    await settle();
    expect(forge.refreshed, 'the return queued a second read behind one in flight').toEqual([
      BOARD,
    ]);

    // The restarted beat's first fire lands while that read is still out.
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat queued behind the read in flight').toEqual([BOARD]);

    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the beat did not ask once the read was answered').toEqual([
      BOARD,
      BOARD,
    ]);
    stop();
  });

  /**
   * **Android's only resume is the shell's event.** It reports neither focus
   * nor reliably a visibility change, so a beat stopped by a hidden document
   * comes back through `client://foreground` or not at all.
   */
  it('restarts the beat on the shell resume a phone reports instead of focus', async () => {
    (window as unknown as Record<string, unknown>)['__TAURI_INTERNALS__'] = {};
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    // The listener is installed behind a dynamic import, so it lands a
    // microtask after the store is watched.
    await vi.advanceTimersByTimeAsync(0);
    expect(foreground, 'the shell resume listener is installed under a shell').toHaveLength(1);

    hidden = true;
    document.dispatchEvent(new Event('visibilitychange'));
    vi.advanceTimersByTime(MARK_BEAT_MS * 2);
    expect(forge.refreshed, 'a hidden page was read on the beat').toEqual([]);

    // No focus and no visibility change: the shell's event is the only word.
    foreground[0]?.({});
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the shell resume did not restart the beat').toEqual([BOARD]);

    stop();
    expect(stopFor('client://foreground'), 'the shell listener is given back').toHaveBeenCalled();
  });

  /**
   * **A foreground report can land after the last reader left.** The shell's
   * resume is a promise and its disconnect may not have completed, so the arm
   * must not start a beat for a store nobody holds - an interval nothing ever
   * clears.
   */
  it('starts no beat from a foreground report that lands after the watch ended', async () => {
    (window as unknown as Record<string, unknown>)['__TAURI_INTERNALS__'] = {};
    const forge = fakeConnection();
    const board = watchBoard(forge.connection);
    const stop = board.subscribe(() => {});
    await vi.advanceTimersByTimeAsync(0);
    const resume = foreground[0];
    expect(resume, 'the shell resume listener is installed under a shell').toBeDefined();

    focused = false;
    stop();
    resume?.({});
    expect(vi.getTimerCount(), 'a beat was armed for a store nobody holds').toBe(0);

    // A board taken again while the page is unwatched stays quiet: a leftover
    // interval would read it as soon as this subscribe gave it a subject.
    const again = board.subscribe(() => {});
    vi.advanceTimersByTime(MARK_BEAT_MS * 3);
    expect(forge.refreshed, 'a leftover beat read a page nobody watches').toEqual([]);
    again();
  });

  /**
   * **A coalesced update is a read of a page that is still being watched.**
   * Once the last reader leaves there is nobody to draw it, so the pending
   * timer goes with the watch.
   */
  it('drops a coalesced read when the last reader leaves', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    stop();
    await settle();
    expect(forge.refreshed, 'a coalesced read outlived the last reader').toEqual([]);
  });

  /**
   * **A drop takes the read in flight with it.** Nothing will answer a read
   * the socket carried away, so the next update must ask again rather than
   * be folded into a read that is never coming.
   */
  it('asks again for an update that lands after a reconnect', async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});

    forge.arrive(UPDATE);
    await settle();
    expect(forge.refreshed, 'the update did not ask for its read').toEqual([BOARD]);

    forge.setStatus('connecting');
    forge.setStatus('open');
    forge.arrive(NEWS);
    await settle();
    expect(forge.refreshed, 'the update after a reconnect was dropped').toEqual([BOARD, BOARD]);
    stop();
  });

  /**
   * **The dedupe is per project.** One key for every project would make each
   * project's announcement look like a change to the last one's set, which is
   * the read-per-announcement loop #2540 removed.
   */
  it("dedupes each project's task set on its own key", async () => {
    const forge = fakeConnection();
    const stop = watchBoard(forge.connection).subscribe(() => {});
    const other: SessionSlot = { org: 'TestOrg', project: 'other', label: 'lead' };
    const first = { id: 't1', project_name: 'proj', subject: 'a row', status: 'pending' };
    const second = { id: 't2', project_name: 'other', subject: 'another row', status: 'pending' };
    const mine: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: LEAD, tasks: [first as unknown as Task] } },
    };
    const theirs: ServerMessage = {
      kind: 'update',
      update: { tasks_changed: { key: other, tasks: [second as unknown as Task] } },
    };

    forge.arrive(mine);
    await settle();
    expect(forge.refreshed, 'the first project did not ask for a read').toEqual([BOARD]);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });

    forge.arrive(theirs);
    await settle();
    expect(forge.refreshed, 'the second project did not ask for a read').toEqual([BOARD, BOARD]);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });

    forge.arrive(mine);
    await settle();
    expect(forge.refreshed, 'an unchanged set asked again under another key').toEqual([
      BOARD,
      BOARD,
    ]);
    stop();
  });

  /**
   * **A later subscribe is a fresh watch.** The observer goes with the last
   * reader, so a store taken again must install it again and read the page's
   * attention afresh rather than trust the state the last watch left.
   */
  it('re-registers the observer and re-seeds the watched state on a later subscribe', () => {
    const forge = fakeConnection();
    const board = watchBoard(forge.connection);

    // A resubscribe while the page is watched starts the beat from the seed,
    // with no event to tell it so.
    const first = board.subscribe(() => {});
    first();
    const second = board.subscribe(() => {});
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'a resubscribed board did not seed the watched state').toEqual([BOARD]);
    forge.arrive({ kind: 'snapshot', subject: BOARD, data: { projects: [] } });
    second();

    // A resubscribe while it is not watched installs the observer again, so
    // the next real report starts the beat.
    focused = false;
    const third = board.subscribe(() => {});
    vi.advanceTimersByTime(MARK_BEAT_MS * 2);
    expect(forge.refreshed, 'a board resubscribed unwatched was read on the beat').toEqual([BOARD]);

    focused = true;
    window.dispatchEvent(new Event('focus'));
    vi.advanceTimersByTime(MARK_BEAT_MS);
    expect(forge.refreshed, 'the observer was not re-registered').toEqual([BOARD, BOARD]);
    third();
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

    vi.advanceTimersByTime(MARK_BEAT_MS);
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
    vi.advanceTimersByTime(MARK_BEAT_MS - 10);
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

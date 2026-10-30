// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { PROTOCOL_VERSION, type Subject } from '../protocol';
import { rereadSeat } from '../session/live';
import type { Connection, ConnectionStatus } from '../socket';
import { Stores } from '../stores';
import type { SessionSlot } from '../wire/types';
import { resync, watchReturn } from './resync';

/**
 * **The seat's half is `session/live`'s own reread**, which its tests pin;
 * what this file is for is the composition - that a return asks the seat it
 * shows, once, and only when the socket can answer.
 */
vi.mock('../session/live', () => ({ rereadSeat: vi.fn() }));

/**
 * The shell's resume listener, captured rather than registered: the event
 * the Rust half emits is the phone's only return signal, and a test drives
 * it from here.
 */
const { foreground } = vi.hoisted(() => ({ foreground: [] as (() => void)[] }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, fn: () => void) => {
    if (name === 'client://foreground') foreground.push(fn);
    return Promise.resolve(() => {});
  },
}));

/** Pretend this page runs inside the shell, so `canHost` says yes. */
function inShell(on: boolean): void {
  const held = window as unknown as Record<string, unknown>;
  if (on) held['__TAURI_INTERNALS__'] = {};
  else delete held['__TAURI_INTERNALS__'];
}

const LEAD: SessionSlot = { org: 'TestOrg', project: 'proj', label: 'lead' };

/**
 * A connection that records the asks a return makes, with the real stores so
 * "the subjects this connection holds" is the same question the page asks.
 */
function fakeConnection() {
  const stores = new Stores();
  const statuses = new Set<(status: ConnectionStatus) => void>();
  const refreshed: Subject[] = [];
  let probed = 0;
  let status: ConnectionStatus = 'open';

  const connection: Connection = {
    subscribe(what) {
      return stores.open(what);
    },
    unsubscribe(what) {
      return stores.close(what);
    },
    refresh(what) {
      refreshed.push(what);
      return true;
    },
    relist() {},
    declare() {},
    release() {},
    showing() {},
    dispatch: () => Promise.resolve(null),
    more: () => false,
    commitDiff: () => false,
    fileContent: () => false,
    readDocument: () => false,
    seatRange: () => false,
    frame: () => false,
    browserInput: () => false,
    onBrowserFrame: () => () => {},
    onMessage: () => () => {},
    onStatus(fn) {
      statuses.add(fn);
      return () => {
        statuses.delete(fn);
      };
    },
    store: (what) => stores.get(what),
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => status,
    probe: () => {
      probed += 1;
    },
    close: () => {},
  };

  return {
    connection,
    stores,
    refreshed,
    probed: () => probed,
    setStatus(next: ConnectionStatus) {
      status = next;
      for (const fn of [...statuses]) fn(next);
    },
  };
}

/** Whether the document reports itself hidden, as the browser would. */
function setHidden(hidden: boolean): void {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
}

/** The page's return, as the browser fires it. */
function visibilityChanged(): void {
  document.dispatchEvent(new Event('visibilitychange'));
}

/**
 * A hidden spell long enough to count, then the return.
 *
 * The clock is the fake one, so `Date.now` moves with the advancing timers
 * and the two-second gate is driven rather than slept through.
 */
async function returnAfter(ms: number): Promise<void> {
  setHidden(true);
  visibilityChanged();
  await vi.advanceTimersByTimeAsync(ms);
  setHidden(false);
  visibilityChanged();
}

describe('the return re-read', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    setHidden(false);
    vi.mocked(rereadSeat).mockClear();
  });

  afterEach(() => {
    vi.useRealTimers();
    setHidden(false);
    inShell(false);
    foreground.length = 0;
  });

  /**
   * **A return asks once for everything the hide made stale**: the probe
   * first, then each subject this connection holds, then the seat on screen.
   */
  it('freshens home, board, usage and the shown seat once on a return', async () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');
    fake.connection.subscribe('board');
    fake.connection.subscribe('usage');
    fake.refreshed.length = 0;

    await returnAfter(5_000);

    expect(fake.probed(), 'the probe goes first, before the reads').toBe(1);
    expect(fake.refreshed).toEqual<Subject[]>(['home', 'board', 'usage']);
    expect(vi.mocked(rereadSeat).mock.calls, 'the seat shown is re-read').toEqual([
      [fake.connection, LEAD],
    ]);
    stop();
  });

  /** A subject this connection does not hold is not asked for. */
  it('asks only for the subjects the connection holds', async () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => null);
    fake.connection.subscribe('home');

    await returnAfter(5_000);

    expect(fake.refreshed).toEqual<Subject[]>(['home']);
    expect(vi.mocked(rereadSeat), 'no seat is shown, so none is re-read').not.toHaveBeenCalled();
    stop();
  });

  /**
   * **A glance at another window is not a return.** The seat read is
   * megabytes, and a hide too short to have lost anything buys nothing.
   */
  it('does nothing when the hide was under two seconds', async () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');

    await returnAfter(1_000);

    expect(fake.refreshed, 'a short hide asks nothing').toEqual([]);
    expect(fake.probed(), 'and probes nothing').toBe(0);
    stop();
  });

  /** A focus without a hide is an ordinary desktop focus change. */
  it('does nothing on a focus without a hide', () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');

    window.dispatchEvent(new Event('focus'));
    document.dispatchEvent(new Event('visibilitychange'));

    expect(fake.refreshed).toEqual([]);
    stop();
  });

  /** Two triggers naming one return buy one read. */
  it('does not double-read when two returns land together', async () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');

    await returnAfter(5_000);
    // A second visible signal with no hide in between.
    visibilityChanged();

    expect(fake.refreshed, 'the second signal is the same return').toEqual<Subject[]>(['home']);
    stop();
  });

  /**
   * **The phone's resume is the shell's event, and it needs no hide.** A
   * locked Android window reports no focus and may report no visibility
   * change either, so `client://foreground` is the signal the return re-read
   * hangs on there.
   */
  it('re-reads on the shell saying the app is foreground again', async () => {
    const fake = fakeConnection();
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');
    // The listener is installed behind a dynamic import, so it lands a
    // microtask after the watcher is made.
    await vi.advanceTimersByTimeAsync(0);
    expect(foreground, 'the shell listener is installed under a shell').toHaveLength(1);

    foreground[0]?.();

    expect(fake.refreshed).toEqual<Subject[]>(['home']);
    stop();
  });

  /** A mobile resume beside the page's own visibility return is one return. */
  it('counts a shell resume and a visibility return as one read', async () => {
    const fake = fakeConnection();
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');
    await vi.advanceTimersByTimeAsync(0);

    await returnAfter(5_000);
    foreground[0]?.();

    expect(fake.refreshed, 'the second trigger is the same return').toEqual<Subject[]>(['home']);
    stop();
  });

  /** A return while the socket is down asks nothing: the reconnect's own
   *  re-ask is the fresher read, and a refresh on a dead socket would not
   *  go anywhere anyway. */
  it('asks nothing while the socket is not open', async () => {
    const fake = fakeConnection();
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.connection.subscribe('home');
    fake.setStatus('connecting');

    await returnAfter(5_000);

    expect(fake.refreshed).toEqual([]);
    stop();
  });
});

describe('the resync itself', () => {
  beforeEach(() => {
    vi.mocked(rereadSeat).mockClear();
  });

  it('does nothing at all on a connection that is not open', () => {
    const fake = fakeConnection();
    fake.setStatus('closed');
    fake.connection.subscribe('home');

    resync(fake.connection, LEAD);

    expect(fake.refreshed).toEqual([]);
    expect(fake.probed()).toBe(0);
    expect(vi.mocked(rereadSeat)).not.toHaveBeenCalled();
  });

  /** A shown seat this connection never opened has nothing to ask on. */
  it('asks for the shown seat only when there is one', () => {
    const fake = fakeConnection();
    fake.connection.subscribe('home');

    resync(fake.connection, null);

    expect(fake.refreshed).toEqual<Subject[]>(['home']);
    expect(vi.mocked(rereadSeat)).not.toHaveBeenCalled();
  });
});

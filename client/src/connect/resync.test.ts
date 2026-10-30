// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { rereadBoard } from '../board/live';
import { rereadHome } from '../home/live';
import { PROTOCOL_VERSION } from '../protocol';
import { rereadSeat } from '../session/live';
import type { Connection, ConnectionStatus } from '../socket';
import type { SessionSlot } from '../wire/types';
import { resync, watchReturn } from './resync';

/**
 * **Each page's half is its own reread**, which its tests pin; what this file
 * is for is the composition - that a return asks the probe first, then each
 * page, then the seat it shows, once, and only when the socket can answer.
 */
vi.mock('../home/live', () => ({ rereadHome: vi.fn() }));
vi.mock('../board/live', () => ({ rereadBoard: vi.fn() }));
vi.mock('../session/live', () => ({ rereadSeat: vi.fn() }));

/**
 * The shell's resume listener, captured rather than registered: the event
 * the Rust half emits is the phone's only return signal, and a test drives
 * it from here. Each install's unlisten is kept so a stop can be watched.
 */
const { foreground, stops } = vi.hoisted(() => ({
  foreground: [] as (() => void)[],
  stops: [] as ReturnType<typeof vi.fn>[],
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (name: string, fn: () => void) => {
    if (name === 'client://foreground') foreground.push(fn);
    const stop = vi.fn();
    stops.push(stop);
    return Promise.resolve(stop);
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
 * A connection that records the asks a return makes, with `calls` keeping the
 * ORDER - which is the property the probe-first rule states.
 */
function fakeConnection() {
  const statuses = new Set<(status: ConnectionStatus) => void>();
  const calls: string[] = [];
  let status: ConnectionStatus = 'open';

  const connection: Connection = {
    subscribe() {
      throw new Error('the page rereads are mocked, so nothing subscribes here');
    },
    unsubscribe() {},
    refresh() {
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
    store: () => undefined,
    settings: () => null,
    skew: () => null,
    serverProtocol: () => PROTOCOL_VERSION,
    status: () => status,
    probe: () => {
      calls.push('probe');
    },
    close: () => {},
  };

  return {
    connection,
    calls,
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

/** The page rereads, wired into the fake's ordered calls. */
function pages(fake: ReturnType<typeof fakeConnection>): void {
  vi.mocked(rereadHome).mockImplementation(() => fake.calls.push('home'));
  vi.mocked(rereadBoard).mockImplementation(() => fake.calls.push('board'));
  vi.mocked(rereadSeat).mockImplementation(() => fake.calls.push('seat'));
}

describe('the return re-read', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    setHidden(false);
    vi.mocked(rereadHome).mockReset();
    vi.mocked(rereadBoard).mockReset();
    vi.mocked(rereadSeat).mockReset();
  });

  afterEach(() => {
    vi.useRealTimers();
    setHidden(false);
    inShell(false);
    foreground.length = 0;
    stops.length = 0;
  });

  /**
   * **A return asks once, in one order**: the probe first, then each page
   * through its own guards, then the seat on screen. The order is the claim -
   * a probe that lands after the reads answers for a link the reads may
   * already have waited on.
   */
  it('probes, then asks each page, then re-reads the shown seat', async () => {
    const fake = fakeConnection();
    pages(fake);
    const stop = watchReturn(fake.connection, () => LEAD);

    await returnAfter(5_000);

    expect(fake.calls).toEqual(['probe', 'home', 'board', 'seat']);
    stop();
  });

  /**
   * **A glance at another window is not a return.** The seat read is
   * megabytes, and a hide too short to have lost anything buys nothing.
   */
  it('does nothing when the hide was under two seconds', async () => {
    const fake = fakeConnection();
    pages(fake);
    const stop = watchReturn(fake.connection, () => LEAD);

    await returnAfter(1_000);

    expect(fake.calls, 'a short hide asks nothing').toEqual([]);
    stop();
  });

  /** A focus without a hide is an ordinary desktop focus change. */
  it('does nothing on a focus without a hide', () => {
    const fake = fakeConnection();
    pages(fake);
    const stop = watchReturn(fake.connection, () => LEAD);

    window.dispatchEvent(new Event('focus'));
    document.dispatchEvent(new Event('visibilitychange'));

    expect(fake.calls).toEqual([]);
    stop();
  });

  /** Two triggers naming one return buy one read. */
  it('does not double-read when two returns land together', async () => {
    const fake = fakeConnection();
    pages(fake);
    const stop = watchReturn(fake.connection, () => LEAD);

    await returnAfter(5_000);
    // A second visible signal with no hide in between.
    visibilityChanged();

    expect(vi.mocked(rereadHome), 'the second signal is the same return').toHaveBeenCalledTimes(1);
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
    pages(fake);
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    // The listener is installed behind a dynamic import, so it lands a
    // microtask after the watcher is made.
    await vi.advanceTimersByTimeAsync(0);
    expect(foreground, 'the shell listener is installed under a shell').toHaveLength(1);

    foreground[0]?.();

    expect(fake.calls).toEqual(['probe', 'home', 'board', 'seat']);
    stop();
  });

  /** A mobile resume beside the page's own visibility return is one return. */
  it('counts a shell resume and a visibility return as one read', async () => {
    const fake = fakeConnection();
    pages(fake);
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    await vi.advanceTimersByTimeAsync(0);

    await returnAfter(5_000);
    foreground[0]?.();

    expect(vi.mocked(rereadHome), 'the second trigger is the same return').toHaveBeenCalledTimes(1);
    stop();
  });

  /**
   * **A shell resume spends the hide.** The signal names the return, so a
   * bare visible signal that follows is the same return even after the dedupe
   * window has passed - a stale hidden mark would re-read again.
   */
  it('does not re-read on a bare visible signal after a shell resume', async () => {
    const fake = fakeConnection();
    pages(fake);
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    await vi.advanceTimersByTimeAsync(0);

    setHidden(true);
    visibilityChanged();
    await vi.advanceTimersByTimeAsync(3_000);
    foreground[0]?.();
    await vi.advanceTimersByTimeAsync(3_000);
    setHidden(false);
    visibilityChanged();

    expect(vi.mocked(rereadHome), 'one return, one read').toHaveBeenCalledTimes(1);
    stop();
  });

  /** A return while the socket is down asks nothing: the reconnect's own
   *  re-ask is the fresher read, and a refresh on a dead socket would not
   *  go anywhere anyway. */
  it('asks nothing while the socket is not open', async () => {
    const fake = fakeConnection();
    pages(fake);
    const stop = watchReturn(fake.connection, () => LEAD);
    fake.setStatus('connecting');

    await returnAfter(5_000);

    expect(fake.calls).toEqual([]);
    stop();
  });

  /**
   * **A stopped watcher is dead to both signals**, and it hands its shell
   * listener back: one left behind pins the connection for the page's life.
   */
  it('asks nothing after it is stopped, and gives the listener back', async () => {
    const fake = fakeConnection();
    pages(fake);
    inShell(true);
    const stop = watchReturn(fake.connection, () => LEAD);
    await vi.advanceTimersByTimeAsync(0);
    const unlisten = stops.at(-1);

    stop();

    expect(unlisten, 'the shell listener is given back').toHaveBeenCalled();
    foreground[0]?.();
    setHidden(true);
    visibilityChanged();
    await vi.advanceTimersByTimeAsync(5_000);
    setHidden(false);
    visibilityChanged();
    expect(fake.calls, 'nothing is asked after the stop').toEqual([]);
  });
});

describe('the resync itself', () => {
  beforeEach(() => {
    vi.mocked(rereadHome).mockReset();
    vi.mocked(rereadBoard).mockReset();
    vi.mocked(rereadSeat).mockReset();
  });

  it('does nothing at all on a connection that is not open', () => {
    const fake = fakeConnection();
    pages(fake);
    fake.setStatus('closed');

    resync(fake.connection, LEAD);

    expect(fake.calls).toEqual([]);
    expect(vi.mocked(rereadSeat)).not.toHaveBeenCalled();
  });

  /** The seat is the shown one, and only when there is one. */
  it('asks the seat only when one is shown', () => {
    const fake = fakeConnection();
    pages(fake);

    resync(fake.connection, null);

    expect(fake.calls).toEqual(['probe', 'home', 'board']);
    expect(vi.mocked(rereadSeat)).not.toHaveBeenCalled();
  });
});

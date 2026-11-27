// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { PROTOCOL_VERSION } from '../protocol';
import type { Connection, ConnectionStatus } from '../socket';
import { RETURN_HIDE_MS, resync, watchReturn } from './resync';

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

/** Pretend this page runs inside the shell, so the resume listener installs. */
function inShell(on: boolean): void {
  const held = window as unknown as Record<string, unknown>;
  if (on) held['__TAURI_INTERNALS__'] = {};
  else delete held['__TAURI_INTERNALS__'];
}

/** A connection that records the asks a return makes, in order. */
function fakeConnection() {
  let status: ConnectionStatus = 'open';
  const calls: string[] = [];

  const connection: Connection = {
    subscribe() {
      throw new Error('a return subscribes nothing');
    },
    unsubscribe() {},
    holds: () => false,
    refresh(what) {
      calls.push(`refresh:${typeof what === 'string' ? what : 'seat'}`);
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
    onStatus: () => () => {},
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
    close() {
      status = 'closed';
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

/** A hidden spell long enough to count, then the return. */
async function hideAndReturn(): Promise<void> {
  setHidden(true);
  visibilityChanged();
  await vi.advanceTimersByTimeAsync(RETURN_HIDE_MS + 1);
  setHidden(false);
  visibilityChanged();
}

beforeEach(() => {
  vi.useFakeTimers();
  setHidden(false);
});

afterEach(() => {
  vi.useRealTimers();
  delete (document as unknown as { hidden?: boolean }).hidden;
  delete (window as unknown as Record<string, unknown>)['__TAURI_INTERNALS__'];
  foreground.length = 0;
  stops.length = 0;
});

describe('the return', () => {
  /**
   * **A return probes and reads nothing.** Every held subscription stayed
   * open while the page was hidden, so home, the board and the seats arrived
   * as patches and were applied as they landed; a socket that dropped
   * re-subscribes on reconnect, and that answer is the whole read.
   */
  it('probes the socket once and asks no subject for a read', async () => {
    const forge = fakeConnection();
    const stop = watchReturn(forge.connection);
    await hideAndReturn();
    expect(forge.calls).toEqual(['probe']);
    stop();
  });

  it('does not probe a socket that is not open', () => {
    const forge = fakeConnection();
    forge.close();
    resync(forge.connection);
    expect(forge.calls).toEqual([]);
  });

  /** Two triggers naming one return - a resume beside a visibility change -
   * must not spend two probes. */
  it('counts one return once, however many signals name it', async () => {
    const forge = fakeConnection();
    watchReturn(forge.connection);
    await hideAndReturn();
    foreground.forEach((fire) => fire());
    expect(forge.calls).toEqual(['probe']);
  });

  /** A desktop focus change and a glance away leave the document visible and
   * owe nothing at all. */
  it('does nothing for a short hide', async () => {
    const forge = fakeConnection();
    const stop = watchReturn(forge.connection);
    setHidden(true);
    visibilityChanged();
    await vi.advanceTimersByTimeAsync(100);
    setHidden(false);
    visibilityChanged();
    expect(forge.calls).toEqual([]);
    stop();
  });

  /** Android reports no focus, so the shell's resume is the return there. */
  it('probes on the shell resume a phone reports instead of focus', async () => {
    inShell(true);
    const forge = fakeConnection();
    const stop = watchReturn(forge.connection);
    // The shell's event module is imported lazily, and its `listen` is a
    // promise: wait for the arm to be installed rather than racing it.
    await vi.waitFor(() => expect(foreground.length).toBeGreaterThan(0));
    foreground.forEach((fire) => fire());
    expect(forge.calls).toEqual(['probe']);
    stop();
  });

  it('takes its listeners back when stopped', async () => {
    const forge = fakeConnection();
    const stop = watchReturn(forge.connection);
    stop();
    await hideAndReturn();
    expect(forge.calls).toEqual([]);
  });
});

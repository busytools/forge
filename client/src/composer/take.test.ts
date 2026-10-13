/**
 * One client-captured take, against a socket and a microphone that are
 * fakes: what it dispatches, when it starts, and what it does when the
 * socket is not there.
 */

import { describe, expect, it, vi } from 'vitest';

import { DEFAULT_AXES } from '../session/wire';
import type { Command, ServerMessage } from '../protocol';
import type { ConnectionStatus } from '../socket';
import { encodeFrame } from './capture.svelte';
import {
  DRAIN_MS,
  FLUSH_SETTLE_MS,
  LocalTake,
  RELEASE_WAIT_MS,
  WENT_UNSENT,
  microphoneLine,
  type MicSource,
} from './take';

/** A socket that records what it was told, and can be moved by hand. */
function fakeConnection(open = true) {
  const listeners = new Set<(status: ConnectionStatus) => void>();
  const messages = new Set<(message: ServerMessage) => void>();
  let status: ConnectionStatus = open ? 'open' : 'connecting';
  const sent: Command[] = [];
  const frames: Uint8Array[] = [];
  return {
    sent,
    frames,
    dispatch(command: Command) {
      if (status !== 'open') throw new Error('the socket is not open');
      sent.push(command);
      return null;
    },
    frame(bytes: Uint8Array) {
      if (status !== 'open') return false;
      frames.push(bytes);
      return true;
    },
    status: () => status,
    onStatus(fn: (status: ConnectionStatus) => void) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    onMessage(fn: (message: ServerMessage) => void) {
      messages.add(fn);
      return () => messages.delete(fn);
    },
    move(next: ConnectionStatus) {
      status = next;
      for (const fn of listeners) fn(next);
    },
    push(message: ServerMessage) {
      for (const fn of messages) fn(message);
    },
  };
}

/** A microphone whose frames are handed over by the test. */
function fakeMic() {
  const source: MicSource & {
    stopped: boolean;
    tail: Uint8Array | null;
    asked: number;
    drain?: () => Promise<void>;
  } = {
    onFrame: null,
    stopped: false,
    tail: null,
    asked: 0,
    flush() {
      const tail = source.tail;
      source.tail = null;
      return tail;
    },
    flushWorklet() {
      source.asked += 1;
    },
    stop() {
      source.stopped = true;
    },
  };
  return source;
}

function wiring(connection: ReturnType<typeof fakeConnection>) {
  const lines: string[] = [];
  let ended = 0;
  return {
    lines,
    ended: () => ended,
    seat: { org: 'TestOrg', project: 'proj', label: 'lead' },
    wiring: {
      connection,
      seat: { org: 'TestOrg', project: 'proj', label: 'lead' },
      options: DEFAULT_AXES,
      onLine: (line: string) => lines.push(line),
      onEnded: () => {
        ended += 1;
      },
    },
  };
}

const SEAT = { org: 'TestOrg', project: 'proj', label: 'lead' };

describe('a take on a socket that is up', () => {
  it('starts with the take command, streams, and submits with its stop', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      expect(connection.sent[0]).toEqual({ dictate_stream: { key: SEAT, options: DEFAULT_AXES } });

      const frame = encodeFrame([0.5]);
      mic.onFrame?.(frame);
      expect(connection.frames).toEqual([frame]);

      mic.tail = encodeFrame([0.25]);
      take.stop(true);
      vi.advanceTimersByTime(DRAIN_MS + FLUSH_SETTLE_MS + 10);
      expect(
        connection.frames[1],
        'the tail goes before the stop, so the take has all of it',
      ).toEqual(encodeFrame([0.25]));
      expect(connection.sent[1]).toEqual({ dictate_stop: { key: SEAT, submit: true } });
      expect(mic.stopped).toBe(true);
      expect(w.ended(), 'and the take is over on this side').toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The release keeps the capture open for a drain window before the stop.
   *
   * The browser holds audio of its own at the release - already captured,
   * not yet handed over (measured on the fake-mic rig: 42-53 ms) - and it
   * is only delivered while the graph is still running. The frames have to
   * reach the server BEFORE the stop, because the stop is what finalises
   * the take there, so the drain precedes it.
   */
  it('drains the capture before the stop, so the audio in flight lands', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });
      mic.onFrame?.(encodeFrame([0.5]));

      take.stop(true);
      expect(connection.sent, 'the stop waits for the drain').toHaveLength(1);
      expect(mic.stopped, 'and the capture stays open through it').toBe(false);

      const inFlight = encodeFrame([0.25]);
      mic.onFrame?.(inFlight);
      vi.advanceTimersByTime(DRAIN_MS + FLUSH_SETTLE_MS + 10);
      expect(connection.frames, 'the audio in flight goes before the stop').toEqual([
        encodeFrame([0.5]),
        inFlight,
      ]);
      expect(connection.sent[1]).toEqual({ dictate_stop: { key: SEAT, submit: true } });
      expect(mic.stopped).toBe(true);
      expect(w.ended()).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The worklet holds the newest samples it has not posted yet, and nothing
   * on this side can reach them once the graph is stopped - so the take asks
   * for them before letting go.
   */
  it('asks the worklet for what it still holds, at the end of the drain', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      take.stop(true);
      expect(mic.asked, 'not while the drain is still running').toBe(0);
      vi.advanceTimersByTime(DRAIN_MS + 1);
      expect(mic.asked, 'the ask goes out once the drain has had its window').toBe(1);
      vi.advanceTimersByTime(FLUSH_SETTLE_MS + 10);
      expect(mic.stopped).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * A source that closes itself owns its tail.
   *
   * The desktop's native capture drains its pipeline in the shell's own
   * half, where this side cannot reach it at all - so the take waits for
   * that rather than for its own windows, and the stop still goes out only
   * after the last frame has been handed over.
   */
  it('waits for a source that drains itself before the stop goes out', async () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    // One object rather than a bare binding: the resolver is set inside the
    // promise's own executor, which no control-flow analysis can see.
    const gate: { release: (() => void) | null } = { release: null };
    mic.drain = () =>
      new Promise<void>((resolve) => {
        gate.release = resolve;
      });
    const w = wiring(connection);
    const take = new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });
    mic.onFrame?.(encodeFrame([0.5]));

    take.stop(true);
    expect(connection.sent, 'the stop waits for the source').toHaveLength(1);
    expect(mic.stopped, 'and the capture stays open through it').toBe(false);

    const inFlight = encodeFrame([0.25]);
    mic.onFrame?.(inFlight);
    gate.release?.();
    await Promise.resolve();
    await Promise.resolve();
    expect(connection.frames, 'the frames in flight go before the stop').toEqual([
      encodeFrame([0.5]),
      inFlight,
    ]);
    expect(connection.sent[1]).toEqual({ dictate_stop: { key: SEAT, submit: true } });
    expect(mic.stopped).toBe(true);
    expect(w.ended()).toBe(1);
  });

  /** An abandon during the drain is still an abandon: the audio goes, at once. */
  it('abandons at once when the release is cancelled during the drain', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      take.stop(true);
      take.stop(false);
      expect(connection.sent[1], 'the cancel is dispatched without waiting').toEqual({
        dictate_stop: { key: SEAT, submit: false },
      });
      expect(mic.stopped, 'and the microphone goes with it').toBe(true);
      expect(w.ended()).toBe(1);

      const after = connection.sent.length;
      vi.advanceTimersByTime(DRAIN_MS + FLUSH_SETTLE_MS + 10);
      expect(connection.sent, 'the drained release never fires').toHaveLength(after);
    } finally {
      vi.useRealTimers();
    }
  });

  it('counts its wire live, rather than freezing at the first read', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    const take = new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    const first = encodeFrame([0.5]);
    mic.onFrame?.(first);
    expect(take.wire.frames, 'one frame in').toBe(1);
    expect(take.wire.bytes, 'and the socket took it').toBe(first.length);

    mic.onFrame?.(encodeFrame([0.25]));
    expect(take.wire.frames, 'a second frame, read after the first read').toBe(2);
  });

  it('abandons without sending anything, tail included', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    mic.tail = encodeFrame([0.5]);
    const w = wiring(connection);
    const take = new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    mic.onFrame?.(encodeFrame([0.5]));
    take.stop(false);
    expect(connection.frames, 'nothing goes after an abandon').toHaveLength(1);
    expect(connection.sent[1]).toEqual({ dictate_stop: { key: SEAT, submit: false } });
  });

  it('lets go of the microphone when the server says the take ended', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    connection.push({
      kind: 'update',
      update: { dictate_ended: { key: SEAT, outcome: 'cancelled', generation: 1 } },
    });
    expect(mic.stopped, 'the microphone follows the server, not the key alone').toBe(true);
    expect(w.ended()).toBe(1);
  });

  it("ignores another seat's ending", () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    connection.push({
      kind: 'update',
      update: {
        dictate_ended: {
          key: { org: 'TestOrg', project: 'proj', label: 'worker' },
          outcome: 'cancelled',
          generation: 1,
        },
      },
    });
    expect(mic.stopped).toBe(false);
    expect(w.ended()).toBe(0);
  });

  it('lets go of the microphone when the socket drops mid-take', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    connection.move('connecting');
    expect(mic.stopped, 'the server drops the take on the close, so this side releases').toBe(true);
    expect(w.ended()).toBe(1);
  });

  it('carries the input the stream resolved to, when the source reports one', () => {
    const connection = fakeConnection();
    const w = wiring(connection);
    const heard = {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    };

    const named = new LocalTake(
      { ...fakeMic(), resolved: { id: 'mic-2', label: 'Shure SM7B' } },
      connection,
      DEFAULT_AXES,
      heard,
    );
    expect(named.resolved, 'the panel learns the default from this').toEqual({
      id: 'mic-2',
      label: 'Shure SM7B',
    });

    const bare = new LocalTake(fakeMic(), connection, DEFAULT_AXES, heard);
    expect(bare.resolved, 'a source that reports none leaves it unknown').toBeNull();
  });
});

describe('a take whose socket is down', () => {
  it('holds its frames and starts when the socket returns', () => {
    const connection = fakeConnection(false);
    const mic = fakeMic();
    const w = wiring(connection);
    new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    expect(connection.sent, 'nothing is dispatched into a closed socket').toHaveLength(0);
    mic.onFrame?.(encodeFrame([0.5]));
    mic.onFrame?.(encodeFrame([0.25]));
    expect(connection.frames).toHaveLength(0);

    connection.move('open');
    expect(connection.sent[0]).toEqual({ dictate_stream: { key: SEAT, options: DEFAULT_AXES } });
    expect(connection.frames, 'both held frames go, in the order spoken').toEqual([
      encodeFrame([0.5]),
      encodeFrame([0.25]),
    ]);
  });

  it('submits a release that lands once the socket is back', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection(false);
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      mic.onFrame?.(encodeFrame([0.5]));
      mic.tail = encodeFrame([0.25]);
      take.stop(true);
      expect(w.lines, 'the wait is not over yet').toHaveLength(0);

      connection.move('open');
      expect(connection.sent[1], 'the release follows the start, tail included').toEqual({
        dictate_stop: { key: SEAT, submit: true },
      });
      expect(connection.frames).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('abandons with its own line when the connection never returns', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection(false);
      const mic = fakeMic();
      const w = wiring(connection);
      const take = new LocalTake(mic, connection, DEFAULT_AXES, {
        seat: SEAT,
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      take.stop(true);
      vi.advanceTimersByTime(RELEASE_WAIT_MS + 10);
      expect(w.lines).toEqual([WENT_UNSENT]);
      expect(w.ended()).toBe(1);
      expect(mic.stopped).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * The microphone goes at the GESTURE, not at the end of the wait.
   *
   * A submit with the socket down is held for a connection that may take
   * seconds to arrive, and the whole point of that wait is to SEND what was
   * captured - not to keep capturing. A release that only stopped the
   * microphone when the wait resolved would record past the reader's hand
   * for the length of it.
   */
  it("lets go of the microphone at the gesture, not at the wait's end", () => {
    const connection = fakeConnection(false);
    const mic = fakeMic();
    const w = wiring(connection);
    const take = new LocalTake(mic, connection, DEFAULT_AXES, {
      seat: SEAT,
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    take.stop(true);
    expect(mic.stopped, 'the release must end the recording there and then').toBe(true);
    take.release();
  });
});

describe('the line a microphone that would not open leaves', () => {
  /**
   * The desktop's capture names what it knows: a device id that is gone, a
   * device that will not open, and what else the machine offers. That is the
   * reader's to act on, so it is not flattened into the generic line the
   * browser's unnamed failures get.
   */
  it("keeps the shell's own words about the device", () => {
    expect(
      microphoneLine('no input device named "coreaudio:gone" (available: coreaudio:built-in)'),
    ).toBe(
      'no input device named "coreaudio:gone" (available: coreaudio:built-in) \u{b7} dictation did not start',
    );
  });

  it('reads a browser refusal by its name, and an empty reason not at all', () => {
    expect(microphoneLine(new DOMException('denied', 'NotAllowedError'))).toContain(
      'the microphone was refused',
    );
    expect(microphoneLine('  ')).toBe('the microphone did not open \u{b7} dictation did not start');
  });
});

/**
 * One client-captured take, against a socket and a microphone that are
 * fakes: what it dispatches, when it starts, and what it does when the
 * socket is not there.
 */

import { describe, expect, it, vi } from 'vitest';

import { DEFAULT_AXES } from '../session/wire';
import type { ServerMessage } from '../protocol';
import type { ConnectionStatus } from '../socket';
import { encodeFrame } from './capture';
import { LocalTake, RELEASE_WAIT_MS, WENT_UNSENT, type MicSource } from './take';

type Sent = Record<string, unknown>;

/** A socket that records what it was told, and can be moved by hand. */
function fakeConnection(open = true) {
  const listeners = new Set<(status: ConnectionStatus) => void>();
  const messages = new Set<(message: ServerMessage) => void>();
  let status: ConnectionStatus = open ? 'open' : 'connecting';
  const sent: Sent[] = [];
  const frames: Uint8Array[] = [];
  return {
    sent,
    frames,
    dispatch(command: Sent) {
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
  return {
    onFrame: null as ((bytes: Uint8Array) => void) | null,
    stopped: false,
    tail: null as Uint8Array | null,
    flush() {
      const tail = this.tail;
      this.tail = null;
      return tail;
    },
    stop() {
      this.stopped = true;
    },
  } satisfies MicSource & { stopped: boolean; tail: Uint8Array | null };
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
    expect(
      connection.frames[1],
      'the tail goes before the stop, so the take has all of it',
    ).toEqual(encodeFrame([0.25]));
    expect(connection.sent[1]).toEqual({ dictate_stop: { key: SEAT, submit: true } });
    expect(mic.stopped).toBe(true);
    expect(w.ended(), 'and the take is over on this side').toBe(1);
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

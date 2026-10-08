/**
 * The models page's recording of the read-aloud set, against a socket and a
 * microphone that are fakes: what it dispatches, when it starts, and what it
 * does when the socket is not there.
 */

import { describe, expect, it, vi } from 'vitest';

import { encodeFrame } from '../composer/capture.svelte';
import type { MicSource } from '../composer/take';
import type { Command } from '../protocol';
import type { ConnectionStatus } from '../socket';
import { KEEP_WAIT_MS, SetRecorder, WENT_UNSENT } from './recorder.svelte';

/** A socket that records what it was told, and can be moved by hand. */
function fakeConnection(open = true) {
  const listeners = new Set<(status: ConnectionStatus) => void>();
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
    move(next: ConnectionStatus) {
      status = next;
      for (const fn of listeners) fn(next);
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
    wiring: {
      connection,
      onLine: (line: string) => lines.push(line),
      onEnded: () => {
        ended += 1;
      },
    },
  };
}

describe('a recording on a socket that is up', () => {
  it('starts with the start command, streams, and keeps with its stop', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    const recorder = new SetRecorder(mic, connection, {
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    expect(connection.sent[0]).toEqual({ dictate_read_aloud_start: {} });

    const frame = encodeFrame([0.5]);
    mic.onFrame?.(frame);
    expect(connection.frames).toEqual([frame]);

    const tail = encodeFrame([0.25]);
    mic.tail = tail;
    recorder.stop(true);
    expect(connection.sent[1]).toEqual({ dictate_read_aloud_stop: { keep: true } });
    expect(connection.frames, 'the tail is sent with the rest').toEqual([frame, tail]);
    expect(mic.stopped, 'the microphone is let go at the press').toBe(true);
    expect(w.ended()).toBe(1);
  });

  it('keeps nothing on a cancel: the stop says so and the tail goes nowhere', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    const recorder = new SetRecorder(mic, connection, {
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    mic.tail = encodeFrame([0.25]);
    recorder.stop(false);

    expect(connection.sent[1]).toEqual({ dictate_read_aloud_stop: { keep: false } });
    expect(connection.frames, 'a cancelled recording sends no tail').toEqual([]);
    expect(w.ended()).toBe(1);
  });
});

describe('a recording on a socket that is not up', () => {
  it('holds its frames and starts when the socket comes back', () => {
    const connection = fakeConnection(false);
    const mic = fakeMic();
    const w = wiring(connection);
    new SetRecorder(mic, connection, {
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    expect(connection.sent, 'nothing is sent while the socket is down').toEqual([]);
    const frame = encodeFrame([0.5]);
    mic.onFrame?.(frame);
    expect(connection.frames).toEqual([]);

    connection.move('open');
    expect(connection.sent[0]).toEqual({ dictate_read_aloud_start: {} });
    expect(connection.frames, 'the held frames go with the start').toEqual([frame]);
  });

  it('waits a moment for a keep, then drops it with its own line', () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection(false);
      const mic = fakeMic();
      const w = wiring(connection);
      const recorder = new SetRecorder(mic, connection, {
        onLine: w.wiring.onLine,
        onEnded: w.wiring.onEnded,
      });

      recorder.stop(true);
      expect(w.ended(), 'a keep is held open for the socket').toBe(0);
      vi.advanceTimersByTime(KEEP_WAIT_MS);
      expect(w.lines).toEqual([WENT_UNSENT]);
      expect(w.ended()).toBe(1);
      expect(connection.sent, 'nothing ever reached the socket').toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('lets the microphone go when the connection that owned it does', () => {
    const connection = fakeConnection();
    const mic = fakeMic();
    const w = wiring(connection);
    new SetRecorder(mic, connection, {
      onLine: w.wiring.onLine,
      onEnded: w.wiring.onEnded,
    });

    connection.move('closed');
    expect(mic.stopped, 'the server drops the recording, so this side lets the mic go').toBe(true);
    expect(w.ended()).toBe(1);
  });
});

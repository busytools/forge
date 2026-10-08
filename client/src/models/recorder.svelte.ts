/**
 * The models page's own recording of the read-aloud passage: the composer's
 * capture - the same microphone, the same 20 ms frames, the same ring for a
 * socket that is down - pointed at the set instead of a seat.
 *
 * **A set-recording is not a take.** No session owns it and nothing is
 * transcribed, so the server answers with no `dictate_started` and there is
 * no `dictate_ended` to end this side; the page reads the outcome off the
 * models read, which the server pushes. That leaves this side three things,
 * the same three a take owns: the microphone, the frames, and the claim that
 * the microphone is busy.
 *
 * The input is the system default. A take records from the seat's own picked
 * input, and this page holds no seat to have picked one.
 */

import { FrameRing } from '../composer/capture.svelte';
import { Microphone } from '../composer/mic';
import { microphoneLine, type MicSource } from '../composer/take';
import type { Connection } from '../socket';

/** How long a keep waits for a connection that is not up yet. */
export const KEEP_WAIT_MS = 5_000;

export interface RecorderWiring {
  connection: Pick<Connection, 'dispatch' | 'frame' | 'onStatus' | 'status'>;
  /** A line for the card: a failure the server never saw. */
  onLine: (line: string) => void;
  /** The recording is over on this side, whatever the server last said. */
  onEnded: () => void;
}

/** One recording of the set, while it runs. */
export class SetRecorder {
  /** This side's own take, read live: the levels, the counts and the clock. */
  readonly wire: {
    readonly frames: number;
    readonly bytes: number;
    readonly rate: number;
    readonly dbfs: number[];
    readonly elapsedMs: number;
  };

  private started = false;
  private ended = false;
  private readonly began: { at: number | null } = { at: null };
  private micStopped = false;
  private pendingStop: boolean | null = null;
  private waiting: ReturnType<typeof setTimeout> | null = null;
  private readonly ring: FrameRing;
  private readonly unlisten: () => void;

  constructor(
    private readonly mic: MicSource,
    private readonly connection: RecorderWiring['connection'],
    private readonly wiring: Omit<RecorderWiring, 'connection'>,
  ) {
    this.ring = new FrameRing((bytes) => connection.frame(bytes));
    const ring = this.ring;
    const began = this.began;
    this.wire = {
      get frames(): number {
        return ring.frames;
      },
      get bytes(): number {
        return ring.bytes;
      },
      get rate(): number {
        return ring.rate;
      },
      get dbfs(): number[] {
        return ring.dbfs;
      },
      get elapsedMs(): number {
        return began.at === null ? 0 : Date.now() - began.at;
      },
    };
    mic.onFrame = (bytes) => this.ring.push(bytes);
    this.unlisten = connection.onStatus((status) => this.statusMoved(status));
    if (connection.status() === 'open') this.start();
  }

  /** Open the microphone and begin a recording, or report why it could not. */
  static async begin(wiring: RecorderWiring): Promise<SetRecorder | null> {
    let mic: MicSource;
    try {
      mic = await Microphone.open(null);
    } catch (why) {
      wiring.onLine(microphoneLine(why));
      return null;
    }
    return new SetRecorder(mic, wiring.connection, {
      onLine: wiring.onLine,
      onEnded: wiring.onEnded,
    });
  }

  /**
   * The reader pressed keep or cancel: flush the tail and keep it, or throw
   * the audio away.
   *
   * A keep with the socket down waits [`KEEP_WAIT_MS`] for it, so a press
   * that lands inside a reconnect is not lost; past that the recording is
   * dropped with its own line rather than held open over nothing.
   */
  stop(keep: boolean): void {
    if (this.ended) return;
    const tail = this.mic.flush();
    if (keep && tail !== null) this.ring.push(tail);
    // At the gesture, not at the release: a recording waiting for a
    // connection it never gets must not record past the reader's hand.
    this.stopMic();

    if (this.started) {
      this.sendStop(keep);
      this.release();
      return;
    }
    if (!keep) {
      this.release();
      return;
    }
    this.pendingStop = true;
    this.waiting = setTimeout(() => {
      this.wiring.onLine(WENT_UNSENT);
      this.release();
    }, KEEP_WAIT_MS);
  }

  /** The recording is over on this side. Idempotent. */
  release(): void {
    if (this.ended) return;
    this.ended = true;
    if (this.waiting !== null) clearTimeout(this.waiting);
    this.waiting = null;
    this.stopMic();
    this.unlisten();
    this.wiring.onEnded();
  }

  private stopMic(): void {
    if (this.micStopped) return;
    this.micStopped = true;
    this.mic.onFrame = null;
    this.mic.stop();
  }

  private statusMoved(status: ReturnType<RecorderWiring['connection']['status']>): void {
    if (this.ended) return;
    if (status === 'open') {
      this.start();
      return;
    }
    // The connection that started a recording owns it; when it goes the
    // server drops the recording, and this side lets the microphone go with
    // it rather than recording into nothing.
    if (this.started) this.release();
  }

  /** Dispatch the start, then hand over everything held. Once. */
  private start(): void {
    if (this.started || this.ended) return;
    try {
      void this.connection.dispatch({ dictate_read_aloud_start: {} });
    } catch {
      // Still not open: the next status change comes back here.
      return;
    }
    this.started = true;
    this.began.at = Date.now();
    this.ring.flush();
    if (this.pendingStop !== null) {
      this.pendingStop = null;
      this.sendStop(true);
      this.release();
    }
  }

  private sendStop(keep: boolean): void {
    if (this.connection.status() !== 'open') return;
    try {
      void this.connection.dispatch({ dictate_read_aloud_stop: { keep } });
    } catch {
      // The socket closed between the check and the send; the server drops
      // the recording on the close either way.
    }
  }
}

/** What a keep that never reached the socket says, as a take's own line does. */
export const WENT_UNSENT = 'not connected \u{b7} the recording was not kept';

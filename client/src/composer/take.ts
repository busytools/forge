/**
 * One client-captured take: the microphone, the frames, and what the socket
 * is told.
 *
 * The server drives the take's state - a start is answered with
 * `dictate_started`, and every end with `dictate_ended` - so this side owns
 * exactly three things: the microphone, the frames, and the one claim that
 * the microphone is busy.
 *
 * The server registers the take inside the dispatch that receives
 * `dictate_stream`, so the frames that follow on the same ordered socket
 * always find it. A socket that is down when the key goes down holds its
 * frames in a ring instead, and starts when it is back.
 */

import { slotOf, subjectKey, type ServerMessage } from '../protocol';
import { variantOf } from '../session/apply';
import type { DictateAxes } from '../session/wire';
import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import { FrameRing } from './capture.svelte';

/** How long a release waits for a connection that is not up yet. */
export const RELEASE_WAIT_MS = 5_000;

/**
 * How long the capture stays open past the release.
 *
 * The browser's own input pipeline holds audio that is already captured but
 * not yet handed to the page - measured at 42-53 ms of it at the release,
 * on the fake-mic rig - and it only arrives while the graph is still
 * running. This window is that with room for a slower pipeline; what lands
 * in it is room tone unless the reader is still speaking.
 */
export const DRAIN_MS = 150;

/**
 * How long the drain waits for the worklet's held samples after asking.
 *
 * A bound rather than an expected length: the ask crosses to the audio
 * thread and its answer comes back as a message, a few milliseconds.
 */
export const FLUSH_SETTLE_MS = 50;

/** What a take needs of a microphone, so a test can stand one in. */
export interface MicSource {
  onFrame: ((bytes: Uint8Array) => void) | null;
  flush(): Uint8Array | null;
  /** Ask the source to hand over what it is still holding. */
  flushWorklet(): void;
  /**
   * Close the capture, letting the audio already in flight land first.
   *
   * A source that can do this itself says so: the desktop's native capture
   * lives in the shell's own half, which knows its pipeline and hands over
   * its last frames before it releases the device. A browser's microphone
   * has no such call - the take drains it with its own windows instead -
   * so this is absent there and resolves once the last frame has been
   * handed to `onFrame` where it is present.
   */
  drain?(): Promise<void>;
  stop(): void;
  /** The input the stream opened, when the source reports one. */
  resolved?: { id: string; label: string };
}

/** What a take needs of the socket. */
export type TakeConnection = Pick<
  Connection,
  'dispatch' | 'frame' | 'onMessage' | 'onStatus' | 'status'
>;

interface TakeWiring {
  connection: TakeConnection;
  seat: SessionSlot;
  options: DictateAxes;
  /** The picked input, or `null` for the system default. */
  device: string | null;
  /**
   * How this leg opens its microphone: the browser's own stream, or the
   * shell's native capture. Everything after the open is one path.
   */
  capture: (device: string | null) => Promise<MicSource>;
  /** A line for the notice row: a failure the server never saw. */
  onLine: (line: string) => void;
  /** The take is over on this side, whatever the server last said. */
  onEnded: () => void;
}

/** One live take. */
export class LocalTake {
  readonly seat: SessionSlot;
  /** The input the take's stream opened, when the source reported one. */
  readonly resolved: { id: string; label: string } | null;
  /**
   * The take's wire side, read live: frames produced, bytes the socket has
   * taken, the pace it is taking them at, and the levels and clock of this
   * side's own take. One stable object whose getters read the ring's own
   * signals, so a panel that holds it repaints as the numbers move.
   *
   * The levels and the clock are read here rather than off the record: the
   * core meters the frames it receives and echoes them back at its own 50 ms
   * cadence, where this side has the audio before it encodes it. Same measure
   * over the same frames, read where the frames are made.
   */
  readonly wire: {
    readonly frames: number;
    readonly bytes: number;
    readonly rate: number;
    readonly dbfs: number[];
    readonly elapsedMs: number;
    /**
     * Whether the capture is producing real samples yet. A take opens its
     * microphone before it can record anything with it, and the row says
     * `ready` rather than `recording` until this is true.
     */
    readonly flowing: boolean;
  };

  private started = false;
  /**
   * When this side started the take, which is its own clock's zero - held as
   * one object so the wire's getter can read it without aliasing `this`.
   */
  private readonly began: { at: number | null } = { at: null };
  private ended = false;
  /** Whether the microphone has been let go of, so it happens exactly once. */
  private micStopped = false;
  private pendingStop: boolean | null = null;
  private waiting: ReturnType<typeof setTimeout> | null = null;
  /** A release is draining the pipeline before it closes the take. */
  private draining = false;
  private drain: ReturnType<typeof setTimeout> | null = null;
  private readonly ring: FrameRing;
  private readonly unlisten: () => void;
  private readonly unlistenMessages: () => void;

  constructor(
    private readonly mic: MicSource,
    private readonly connection: TakeConnection,
    private readonly options: DictateAxes,
    private readonly wiring: Omit<TakeWiring, 'connection' | 'options' | 'device' | 'capture'>,
  ) {
    this.seat = wiring.seat;
    this.resolved = mic.resolved ?? null;
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
      get flowing(): boolean {
        return ring.flowing;
      },
    };
    mic.onFrame = (bytes) => this.ring.push(bytes);
    this.unlisten = connection.onStatus((status) => this.statusMoved(status));
    // The server's word ends the take: `dictate_ended` is what says the
    // take resolved - landed, refused, or cut at the cap - and the
    // microphone must let go when it does.
    this.unlistenMessages = connection.onMessage((message) => this.messageArrived(message));
    if (connection.status() === 'open') this.start();
  }

  /** Open the microphone and begin a take, or report why it could not. */
  static async begin(wiring: TakeWiring): Promise<LocalTake | null> {
    let mic: MicSource;
    try {
      mic = await wiring.capture(wiring.device);
    } catch (why) {
      wiring.onLine(microphoneLine(why));
      return null;
    }
    return new LocalTake(mic, wiring.connection, wiring.options, {
      seat: wiring.seat,
      onLine: wiring.onLine,
      onEnded: wiring.onEnded,
    });
  }

  /**
   * The reader let go: flush the tail and submit, or throw the audio away.
   *
   * A submit with the socket down waits [`RELEASE_WAIT_MS`] for it, so a
   * release that lands inside a reconnect is not lost; past that the take is
   * abandoned with its own line rather than held open over nothing.
   */
  stop(submit: boolean): void {
    if (this.ended) return;
    // A release of a take the server knows: the pipeline drains first, so
    // the audio it is still holding lands before the take is closed.
    if (submit && this.started) {
      this.drainThenStop();
      return;
    }
    // The tail only when this take is being kept: an abandon throws its
    // audio away, and the last 20 ms is audio like the rest.
    const tail = this.mic.flush();
    if (submit && tail !== null) this.ring.push(tail);
    // At the gesture, not at the release: a take waiting for a connection it
    // never gets must not record past the reader's hand for the whole wait.
    this.stopMic();

    if (this.started) {
      this.sendStop(submit);
      this.release();
      return;
    }
    if (!submit) {
      this.release();
      return;
    }
    this.pendingStop = true;
    this.waiting = setTimeout(() => {
      this.wiring.onLine(WENT_UNSENT);
      this.release();
    }, RELEASE_WAIT_MS);
  }

  /**
   * Hold the capture open for the drain, then close the take.
   *
   * The browser holds audio of its own when the key comes up - captured but
   * not yet handed over, measured at 42-53 ms of it on the fake-mic rig -
   * and it only arrives while the graph is still running. The stop goes out
   * after it, because the server finalises the take the moment the stop
   * lands: a frame that follows it has no take to land in.
   */
  private drainThenStop(): void {
    if (this.draining) return;
    this.draining = true;
    // A source that closes itself owns its own tail: the desktop's native
    // capture drains its pipeline in the shell's half and hands over its
    // last frames, which this side could not reach at all.
    if (this.mic.drain !== undefined) {
      void this.mic.drain().then(() => this.finish());
      return;
    }
    this.drain = setTimeout(() => {
      // The worklet holds the newest samples of all, and nothing on this
      // side can reach them once the graph is stopped.
      this.mic.flushWorklet();
      this.drain = setTimeout(() => this.finish(), FLUSH_SETTLE_MS);
    }, DRAIN_MS);
  }

  /** Close a submitted take: its last partial frame, the microphone, the stop. */
  private finish(): void {
    if (this.ended) return;
    const tail = this.mic.flush();
    if (tail !== null) this.ring.push(tail);
    this.stopMic();
    this.sendStop(true);
    this.release();
  }

  /**
   * Whether a release is draining the capture before it closes the take.
   *
   * A press inside that window has nothing to start and nothing to stop: the
   * take it would refuse is the one the reader just let go of, and it frees
   * itself in a moment.
   */
  get closing(): boolean {
    return this.draining;
  }

  /** The take is over: by the server's word, or by a drop. Idempotent. */
  release(): void {
    if (this.ended) return;
    this.ended = true;
    if (this.waiting !== null) clearTimeout(this.waiting);
    if (this.drain !== null) clearTimeout(this.drain);
    this.waiting = null;
    this.drain = null;
    this.stopMic();
    this.unlisten();
    this.unlistenMessages();
    this.wiring.onEnded();
  }

  /**
   * End this side's take on the microphone, once. The device itself stays
   * held for the next press; what stops here is the reading.
   *
   * Both halves matter: the frames stop being read - a frame posted after
   * the take is over would be pushed into a released ring and sent, which
   * is audio for a take that has ended - and the source is told the take
   * is over, which is what stops the browser's graph or ends the shell's
   * recording without closing the device.
   */
  private stopMic(): void {
    if (this.micStopped) return;
    this.micStopped = true;
    this.mic.onFrame = null;
    this.mic.stop();
  }

  /** The seat's own updates: the server's end closes this side's take. */
  private messageArrived(message: ServerMessage): void {
    if (this.ended || message.kind !== 'update') return;
    const [name] = variantOf(message.update);
    if (name !== 'dictate_ended') return;
    const at = slotOf(message.update);
    if (at === null) return;
    // Keyed by the subject rather than compared structurally: a seat is an
    // object off the wire, and two of them are the same seat when their key
    // says so.
    if (subjectKey({ session: this.seat }) !== subjectKey({ session: at })) return;
    this.release();
  }

  private statusMoved(status: ReturnType<TakeConnection['status']>): void {
    if (this.ended) return;
    if (status === 'open') {
      this.start();
      return;
    }
    // Anything but open mid-take: the server's close handler DROPS the
    // take - its reader is gone, so nothing lands - and this side releases
    // the microphone.
    if (this.started) this.release();
  }

  /** Dispatch the start, then hand over everything held. Once. */
  private start(): void {
    if (this.started || this.ended) return;
    try {
      void this.connection.dispatch({
        dictate_stream: { key: this.seat, options: this.options },
      });
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

  private sendStop(submit: boolean): void {
    if (this.connection.status() !== 'open') return;
    try {
      void this.connection.dispatch({ dictate_stop: { key: this.seat, submit } });
    } catch {
      // The socket closed between the check and the send; the server drops
      // the take on the close either way.
    }
  }
}

/** What a microphone that would not open says, in the vocabulary kept from the
 *  view this client replaced. */
export function microphoneLine(why: unknown): string {
  // The shell names its own failures in words - a device id that is gone, a
  // device that will not open - and they are the reader's to act on, so they
  // pass through; a browser's failures are DOMExceptions, read by name below.
  if (typeof why === 'string' && why.trim() !== '') {
    return `${why} \u{b7} dictation did not start`;
  }
  const name = why instanceof DOMException ? why.name : '';
  switch (name) {
    case 'NotAllowedError':
    case 'SecurityError':
      return 'the microphone was refused \u{b7} allow it for this page and try again';
    case 'NotFoundError':
      return 'no input device is available \u{b7} dictation did not start';
    case 'OverconstrainedError':
      return 'that input is not available \u{b7} pick another microphone';
    case 'NotReadableError':
    case 'AbortError':
      return 'the microphone did not open \u{b7} another app may be holding it';
    default:
      return 'the microphone did not open \u{b7} dictation did not start';
  }
}

/** A release whose connection never came back. */
export const WENT_UNSENT = 'not connected \u{b7} dictation did not start';

/**
 * A start refused because this client already records on another seat.
 *
 * The seat is named the way the view this client replaced named it - the whole
 * slot - so a reader with two seats open knows which one holds the microphone.
 */
export function busyLine(seat: SessionSlot): string {
  const where = `${seat.org}/${seat.project}/${seat.label}`;
  return `the microphone is in use by session ${where} \u{b7} dictation did not start`;
}

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
import { FrameRing } from './capture';
import { Microphone } from './mic';

/** How long a release waits for a connection that is not up yet. */
export const RELEASE_WAIT_MS = 5_000;

/** What a take needs of a microphone, so a test can stand one in. */
export interface MicSource {
  onFrame: ((bytes: Uint8Array) => void) | null;
  flush(): Uint8Array | null;
  stop(): void;
}

/** What a take needs of the socket. */
export type TakeConnection = Pick<
  Connection,
  'dispatch' | 'frame' | 'onMessage' | 'onStatus' | 'status'
>;

export interface TakeWiring {
  connection: TakeConnection;
  seat: SessionSlot;
  options: DictateAxes;
  /** The picked input, or `null` for the system default. */
  device: string | null;
  /** A line for the notice row: a failure the server never saw. */
  onLine: (line: string) => void;
  /** The take is over on this side, whatever the server last said. */
  onEnded: () => void;
}

/** One live take. */
export class LocalTake {
  readonly seat: SessionSlot;

  private started = false;
  private ended = false;
  /** Whether the microphone has been let go of, so it happens exactly once. */
  private micStopped = false;
  private pendingStop: boolean | null = null;
  private waiting: ReturnType<typeof setTimeout> | null = null;
  private readonly ring: FrameRing;
  private readonly unlisten: () => void;
  private readonly unlistenMessages: () => void;

  constructor(
    private readonly mic: MicSource,
    private readonly connection: TakeConnection,
    private readonly options: DictateAxes,
    private readonly wiring: Omit<TakeWiring, 'connection' | 'options' | 'device'>,
  ) {
    this.seat = wiring.seat;
    this.ring = new FrameRing((bytes) => connection.frame(bytes));
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
    let mic: Microphone;
    try {
      mic = await Microphone.open(wiring.device);
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

  /** The take is over: by the server's word, or by a drop. Idempotent. */
  release(): void {
    if (this.ended) return;
    this.ended = true;
    if (this.waiting !== null) clearTimeout(this.waiting);
    this.waiting = null;
    this.stopMic();
    this.unlisten();
    this.unlistenMessages();
    this.wiring.onEnded();
  }

  /**
   * Let go of the microphone, once.
   *
   * Both halves matter: the device is released, and the frames stop being
   * read - a frame posted after the take is over would be pushed into a
   * released ring and sent, which is audio for a take that has ended.
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
      // The socket closed between the check and the send; the server
      // submits on the close either way.
    }
  }
}

/** What a microphone that would not open says, in the terminal's vocabulary. */
export function microphoneLine(why: unknown): string {
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
 * The seat is named the way the terminal names it - the whole slot - so a
 * reader with two seats open knows which one holds the microphone.
 */
export function busyLine(seat: SessionSlot): string {
  const where = `${seat.org}/${seat.project}/${seat.label}`;
  return `the microphone is in use by session ${where} \u{b7} dictation did not start`;
}

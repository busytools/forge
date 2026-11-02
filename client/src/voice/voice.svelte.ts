/**
 * One seat's voice, as this client holds it: whether this client has it on,
 * the hold that becomes an utterance, and the speech coming back.
 *
 * **The mode belongs to the client, not the seat's record.** Two clients can
 * watch one seat and only one of them be listening, so nothing here is
 * derived from the conversation: the switch is this connection's own, and
 * the count of listeners is the seat's answer to it.
 */

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import type { SessionUpdate } from '../protocol';
import { MAX_HOLD_SECONDS, VoiceCapture, type HeldClip } from './capture.svelte';
import { Playback, type Speaker } from './playback.svelte';

/** What one seat's control can show. */
export type VoiceState = 'off' | 'listening' | 'talking' | 'speaking';

/** How the driver reaches the core: one command out, the seat's frames in. */
export interface VoicePort {
  send(command: 'voice_mode', body: { on: boolean }): void;
  send(command: 'voice_utterance', body: { pcm: number[] }): void;
  /** The seat's frames since the last call, in arrival order. */
  frames(): SessionUpdate[];
}

/** How a hold opens a microphone; a seam so a test needs no audio stack. */
export type OpenCapture = () => Promise<VoiceCapture>;

export class Voice {
  /** Whether this client has voice on for the seat. */
  on = $state(false);
  /** Clients with voice on for the seat, this one included. */
  listeners = $state(0);
  /** A line the control draws: a failed open, or a hold that was cut. */
  line = $state<string | null>(null);
  /** The hold in progress, so the control can draw its level and clock. */
  clip = $state.raw<HeldClip | null>(null);

  private capture: VoiceCapture | null = null;
  private readonly playback: Playback;
  /**
   * Whether the microphone is still opening. A press during the open is not
   * a second open, and a release or cancel during it must not be a no-op.
   */
  #opening = false;
  /**
   * Counts opens. A device that arrives after its hold ended - the release,
   * cancel or unmount landed while the permission prompt was up - is closed
   * on arrival rather than left capturing into a clip nobody will release.
   */
  #openToken = 0;

  constructor(
    private readonly port: VoicePort,
    private readonly open: OpenCapture = () => VoiceCapture.open(null),
    speaker?: Speaker,
  ) {
    this.playback = speaker === undefined ? new Playback() : new Playback(speaker);
  }

  get state(): VoiceState {
    if (this.capture !== null) return 'talking';
    if (this.playback.playing) return 'speaking';
    return this.on ? 'listening' : 'off';
  }

  turnOn(): void {
    if (this.on) return;
    this.on = true;
    this.line = null;
    this.port.send('voice_mode', { on: true });
  }

  turnOff(): void {
    this.cancelTalk();
    this.playback.drop();
    if (!this.on) return;
    this.on = false;
    this.port.send('voice_mode', { on: false });
  }

  /** Begin a hold. A press on a switched-off control turns it on instead. */
  async startTalk(): Promise<void> {
    if (!this.on) {
      this.turnOn();
      return;
    }
    if (this.capture !== null || this.#opening) return;
    const token = ++this.#openToken;
    this.#opening = true;
    try {
      const capture = await this.open();
      // The hold ended while the device opened: the release, cancel or seat
      // change already bumped the token, so close it rather than holding a
      // microphone nobody is holding. Nothing was captured before the open
      // resolved, so there is no clip to lose.
      if (token !== this.#openToken || !this.on) {
        capture.cancel();
        return;
      }
      this.capture = capture;
      this.clip = capture.clip;
      this.line = null;
    } catch (why) {
      // A failure for an open whose hold is already gone is not the
      // reader's to see: they cancelled, and the next hold is a new one.
      if (token === this.#openToken) this.line = micFailure(why);
    } finally {
      this.#opening = false;
    }
  }

  /** End the hold: the whole clip crosses as one utterance. */
  async endTalk(): Promise<void> {
    // A release while the device opens means the hold is already over. The
    // open's device closes on arrival instead of starting a clip the next
    // press would send.
    if (this.#opening) {
      this.#openToken += 1;
      return;
    }
    const capture = this.capture;
    if (capture === null) return;
    this.capture = null;
    this.clip = null;
    const clip = await capture.release();
    if (clip.capped) this.line = `that hold ran past ${MAX_HOLD_SECONDS}s and was cut there`;
    const pcm = clip.take();
    if (pcm.length > 0) this.port.send('voice_utterance', { pcm: Array.from(pcm) });
  }

  /** Throw the hold away: nothing is sent. */
  cancelTalk(): void {
    // A cancel while the device opens closes it on arrival: the open's
    // device must not outlive the hold it was opened for.
    this.#openToken += 1;
    const capture = this.capture;
    this.capture = null;
    this.clip = null;
    capture?.cancel();
  }

  /** Take the seat's new frames: its listener count, and its clips. */
  pump(): void {
    for (const update of this.port.frames()) {
      const state = (update as { voice_mode_state?: { listeners?: unknown } }).voice_mode_state;
      if (state !== undefined) {
        this.listeners = typeof state.listeners === 'number' ? state.listeners : 0;
        // The core says nobody is listening any more - the last other client
        // left, or this connection's own mode was never registered - so the
        // switch follows rather than showing an off seat as on.
        if (this.listeners === 0 && this.on) this.on = false;
      }
      const audio = (
        update as {
          voice_audio?: { seq?: unknown; sample_rate?: unknown; pcm?: unknown };
        }
      ).voice_audio;
      if (audio !== undefined) {
        const pcm = Array.isArray(audio.pcm) ? Int16Array.from(audio.pcm as number[]) : null;
        if (pcm !== null) {
          const rate = typeof audio.sample_rate === 'number' ? audio.sample_rate : 24_000;
          this.playback.push(pcm, rate);
        }
      }
    }
  }

  /**
   * Leave the seat: turn its voice off for this client, then forget what was
   * held. A client showing another seat is no longer listening to this one,
   * so the count the agent was told about has to fall.
   */
  detach(): void {
    if (this.on) this.port.send('voice_mode', { on: false });
    this.reset();
  }

  /**
   * Forget everything this client held: the switch, the listeners and the
   * queue. A reconnect and a seat change both mean the same thing here -
   * the audio was for a seat nobody is showing any more, and the core has
   * already dropped this connection from the listener set.
   */
  reset(): void {
    this.cancelTalk();
    this.playback.drop();
    this.on = false;
    this.listeners = 0;
    this.line = null;
  }
}

/** Why a microphone would not open, in the reader's words. */
export function micFailure(why: unknown): string {
  const name = why instanceof DOMException ? why.name : '';
  if (name === 'NotAllowedError' || name === 'SecurityError') {
    return 'the microphone was refused \u{b7} allow it and hold again';
  }
  if (name === 'NotFoundError')
    return 'no microphone is available \u{b7} connect one and hold again';
  return 'the microphone did not open \u{b7} check the input and hold again';
}

/**
 * The driver's port over one connection and seat.
 *
 * The frames are the seat store's own tail, read with a cursor: the store is
 * what every frame the server sends lands in, and a reader that took the
 * whole tail each time would replay speech it had already played.
 */
export function connectionPort(
  connection: Pick<Connection, 'store' | 'dispatch'>,
  slot: SessionSlot,
): VoicePort {
  let seen = 0;
  let dropped = 0;
  return {
    send(command, body) {
      void connection.dispatch({ [command]: { key: slot, ...body } });
    },
    frames() {
      const store = connection.store({ session: slot });
      if (store === undefined) return [];
      const held = store.updates();
      const lost = store.dropped();
      if (lost > dropped) {
        // The store's ceiling took the oldest frames; the cursor moves back
        // with them, or the frames it lost would be read as the new ones.
        seen = Math.max(0, seen - (lost - dropped));
        dropped = lost;
      }
      if (seen > held.length) seen = 0;
      const fresh = held.slice(seen);
      seen = held.length;
      return fresh;
    },
  };
}

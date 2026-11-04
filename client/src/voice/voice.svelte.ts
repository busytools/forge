/**
 * One seat's voice, as this client holds it: the switch and the speech coming
 * back.
 *
 * The mode belongs to the client, not the seat's record: two clients can
 * watch one seat and only one of them be listening.
 */

import type { Connection } from '../socket';
import type { SessionSlot } from '../wire/types';
import type { SessionUpdate } from '../protocol';
import { Playback, type Speaker } from './playback.svelte';

/** What one seat's control can show. */
export type VoiceState = 'off' | 'listening' | 'speaking';

/** How the driver reaches the core: one command out, the seat's frames in. */
export interface VoicePort {
  send(command: 'voice_mode', body: { on: boolean; explicit: boolean }): void;
  /** The seat's frames since the last call, in arrival order. */
  frames(): SessionUpdate[];
  /** The listener count the seat's last read carried, before any frame. */
  seed(): number;
}

export class Voice {
  /** Whether this client has voice on for the seat. */
  on = $state(false);
  /** Clients with voice on for the seat, this one included. */
  listeners = $state(0);
  /** A line the control draws: a send on a socket that is down. */
  line = $state<string | null>(null);

  private readonly playback: Playback;
  /**
   * Whether this client wants voice on, which survives a dropped socket:
   * the mode resubscribes when the connection comes back.
   */
  #wanted = false;

  constructor(
    private readonly port: VoicePort,
    speaker?: Speaker,
  ) {
    this.playback = speaker === undefined ? new Playback() : new Playback(speaker);
    // A late attach reads the room from the seat's own snapshot rather than
    // showing zero until the next toggle.
    this.listeners = port.seed();
  }

  get state(): VoiceState {
    if (this.playback.playing) return 'speaking';
    return this.on ? 'listening' : 'off';
  }

  /** Clips this seat has begun sounding, ever. A preview press records it;
   * only a later start marks the new press as playing. */
  get started(): number {
    return this.playback.started;
  }

  turnOn(): void {
    if (this.on) return;
    this.#wanted = true;
    this.on = true;
    this.line = null;
    // The switch is a gesture too, and the replies it asks for arrive long
    // after it: the device has to be awake before they do.
    this.unlock();
    if (!this.send({ on: true, explicit: false })) {
      this.on = false;
      this.#wanted = false;
    }
  }

  turnOff(): void {
    this.playback.drop();
    this.#wanted = false;
    if (!this.on) return;
    this.on = false;
    this.send({ on: false, explicit: true });
  }

  /**
   * Create or resume the audio device inside the press's own gesture, so the
   * clip it asks for is audible: a context a browser holds suspended starts
   * its sources silently.
   */
  unlock(): void {
    void this.playback.unlock();
  }

  /**
   * Stop the clip sounding now and drop what queued behind it. A preview press
   * takes this first, so the pressed voice is heard as its first chunk arrives
   * rather than behind the clip already playing.
   */
  stop(): void {
    this.playback.drop();
  }

  /**
   * The connection this voice spoke through is gone for good: stop its audio
   * and forget the server ever counted it. **Nothing is sent** - that server
   * dropped this client from its listener sets when the socket closed, and a
   * send on a closed socket would throw.
   */
  forget(): void {
    this.playback.drop();
    this.#wanted = false;
    this.on = false;
  }

  /** The socket came back: resubscribe if this client still wants voice. */
  resume(): void {
    if (this.#wanted && !this.on) this.turnOn();
  }

  /** Send the switch, turning it off when the socket is down. */
  private send(body: { on: boolean; explicit: boolean }): boolean {
    try {
      this.port.send('voice_mode', body);
      return true;
    } catch {
      this.line = 'the socket is down, so that was not sent';
      return false;
    }
  }

  /** Take the seat's new frames: its listener count, and its clips. */
  pump(): void {
    for (const update of this.port.frames()) {
      // A new occupant: the old one's speech is not this seat's any more.
      if ((update as { session_replaced?: unknown }).session_replaced !== undefined) {
        this.playback.drop();
      }
      const state = (update as { voice_mode_state?: { listeners?: unknown } }).voice_mode_state;
      if (state !== undefined) {
        this.listeners = typeof state.listeners === 'number' ? state.listeners : 0;
        // Known race, two clients: a `listeners: 0` from the other client's
        // off can land after this client's optimistic turn-on, turning the
        // switch off while the server counts this client as listening; the
        // next toggle reconciles.
        if (this.listeners === 0 && this.on) {
          this.on = false;
          this.playback.drop();
        }
      }
      const audio = (
        update as {
          voice_audio?: { seq?: unknown; sample_rate?: unknown; pcm?: unknown };
        }
      ).voice_audio;
      // Speech belongs to a client whose voice is on. A client that is only
      // watching the seat still receives its frames, and playing them would
      // speak into a room that asked for silence.
      if (audio !== undefined && this.on) {
        // The wire's pcm is a JSON number array of i16 samples; the cast is
        // this boundary's own.
        const pcm = Array.isArray(audio.pcm) ? Int16Array.from(audio.pcm as number[]) : null;
        if (pcm !== null) {
          const rate = typeof audio.sample_rate === 'number' ? audio.sample_rate : 24_000;
          this.playback.push(pcm, rate);
        }
      }
    }
  }

  /**
   * The reader moved to another seat: stop this seat's audio and tell the
   * server this client is no longer listening, but keep the intent so a
   * return reconnects.
   */
  pause(): void {
    this.playback.drop();
    if (!this.on) return;
    this.on = false;
    this.send({ on: false, explicit: false });
  }

  /**
   * The connection is gone: drop the switch's on state, the listeners and the
   * queue, keeping the intent so a reconnect resumes. The core has already
   * dropped this connection from the listener set.
   */
  reset(): void {
    this.playback.close();
    this.on = false;
    this.listeners = 0;
    this.line = null;
  }
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
  // `null` until the first read: a port built on a seat that already holds
  // frames - a revisit, or a remount over the same seat store - must start
  // at the end of what is there rather than replay speech the reader already
  // heard. The seed lands on the first `frames()` call, before anything new
  // can arrive, because the store's own subscribe fires synchronously.
  let seen: number | null = null;
  let dropped = 0;
  let generation = 0;
  return {
    send(command, body) {
      void connection.dispatch({ [command]: { key: slot, ...body } });
    },
    seed() {
      const snapshot = connection.store({ session: slot })?.snapshot();
      if (snapshot === null || typeof snapshot !== 'object') return 0;
      const state = (snapshot as { state?: { voice_listeners?: unknown } }).state;
      return typeof state?.voice_listeners === 'number' ? state.voice_listeners : 0;
    },
    frames() {
      const store = connection.store({ session: slot });
      if (store === undefined) return [];
      const held = store.updates();
      const lost = store.dropped();
      const now = store.generation();
      if (seen === null) {
        // Seed at the end of what is there, and drop the audio history: a
        // remount over an old seat store must not replay speech it already
        // heard, and no other reader takes these frames.
        seen = held.length;
        dropped = lost;
        generation = now;
        store.takeAudio();
        return [];
      }
      if (now !== generation) {
        // The tail was reshaped under this cursor - a compaction, or the
        // newest state replacing the one before it - so the index it holds
        // names nothing. Replay the retained tail: the voice states are
        // idempotent and the chat edge is another reader's.
        generation = now;
        dropped = lost;
        seen = 0;
      }
      if (lost > dropped) {
        // The store's ceiling took the oldest frames; the cursor moves back
        // with them, or the frames it lost would be read as the new ones.
        seen = Math.max(0, seen - (lost - dropped));
        dropped = lost;
      }
      if (seen > held.length) seen = 0;
      const fresh = held.slice(seen);
      seen = held.length;
      return [...fresh, ...store.takeAudio()];
    },
  };
}

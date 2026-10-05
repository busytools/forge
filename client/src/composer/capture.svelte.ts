/**
 * The microphone side of a dictation take: the wire encoder, the frame
 * chunker, and the bounded ring a take holds while the socket is down.
 *
 * Ported in VOCABULARY from the server's `transport/frame.rs`: the same
 * one-byte header, the same little-endian i16 at 2^15, and the same 20 ms
 * cadence. The cadence is not free-form - the server's meter polls a
 * take-and-reset peak every 50 ms, so a chunk larger than that would make
 * alternate readings silence and the level bar sawtooth.
 *
 * Pure except for the worklet glue below and the ring's two counters, which
 * are signals so a panel draws them as the take moves: the encoding and the
 * rest of the ring are pinned by tests rather than through a mounted
 * component.
 */

/** The rate the dictation models read, and every frame carries. */
export const SAMPLE_RATE = 16_000;

/** One frame's payload: 20 ms of samples. */
export const FRAME_SAMPLES = 320;

/** The codec tag every frame's header carries, as the server's `Codec`. */
export const CODEC_PCM_I16 = 0;

/** How many frames a take holds while the socket is down: about 30 s. */
export const RING_FRAMES = 1500;

/**
 * Encode `samples` as one frame: the codec tag, then little-endian i16.
 *
 * The scale is 2^15 with the sample clamped first, which is exactly what
 * the server's decode divides by. The two are one contract, and the
 * socket record's fixture is the same vector on both sides.
 */
export function encodeFrame(samples: ArrayLike<number>): Uint8Array {
  const bytes = new Uint8Array(1 + samples.length * 2);
  bytes[0] = CODEC_PCM_I16;
  for (let at = 0; at < samples.length; at += 1) {
    const sample = samples[at] ?? 0;
    const clamped = Math.max(-1, Math.min(1, sample));
    const value = Math.max(-32768, Math.min(32767, Math.round(clamped * 32768)));
    bytes[1 + at * 2] = value & 0xff;
    bytes[2 + at * 2] = (value >> 8) & 0xff;
  }
  return bytes;
}

/**
 * Gathers whatever the microphone posts into whole frames.
 *
 * The worklet posts render-quantum blocks (128 frames, and whatever the
 * context's rate implies), so the 20 ms shape is composed here rather than
 * asked of it. A tail shorter than a frame is emitted as it stands: a frame
 * is a cadence rather than a rule, and padding it would add audio nobody
 * spoke.
 */
export class FrameChunker {
  private held: number[] = [];

  /** Every whole frame these samples complete, in order. */
  push(samples: ArrayLike<number>): Uint8Array[] {
    const frames: Uint8Array[] = [];
    for (let at = 0; at < samples.length; at += 1) {
      this.held.push(samples[at] ?? 0);
      if (this.held.length === FRAME_SAMPLES) {
        frames.push(encodeFrame(this.held));
        this.held = [];
      }
    }
    return frames;
  }

  /** The frames' remainder, as the take's last frame, or `null` when empty. */
  flush(): Uint8Array | null {
    if (this.held.length === 0) return null;
    const tail = encodeFrame(this.held);
    this.held = [];
    return tail;
  }
}

/**
 * Where a take's frames go: the socket while it is up, a bounded ring
 * while it is not.
 *
 * Overflow drops the OLDEST frame, which is what a ring means: a take that
 * outgrows it has lost its own beginning either way, and the speech still
 * being spoken is the part worth keeping.
 */
export class FrameRing {
  private held: Uint8Array[] = [];
  /** Frames the take has PRODUCED, since it began. */
  frames = $state(0);
  /**
   * Bytes the SOCKET has taken, since it began. A frame produced while the
   * socket is down counts in `frames` and not here, which is the difference
   * the pair exists to show: what was spoken and what has left.
   *
   * A frame the ring drops past its cap reads the same way - produced, never
   * taken - so a gap that keeps growing past ~30 s is a socket taking
   * nothing, not one catching up.
   */
  bytes = $state(0);

  constructor(
    private readonly send: (bytes: Uint8Array) => boolean,
    private readonly limit = RING_FRAMES,
  ) {}

  /** Send if nothing is held and the socket takes it, else hold. */
  push(bytes: Uint8Array): void {
    this.frames += 1;
    if (this.held.length === 0 && this.sendNow(bytes)) return;
    this.held.push(bytes);
    if (this.held.length > this.limit) this.held.shift();
  }

  /**
   * Send what is held, oldest first. `false` means the socket went away
   * again with frames still held, which the next flush picks up.
   */
  flush(): boolean {
    while (this.held.length > 0) {
      const next = this.held[0];
      if (next === undefined || !this.sendNow(next)) return false;
      this.held.shift();
    }
    return true;
  }

  /** Send one frame, counting its bytes only when the wire takes it. */
  private sendNow(bytes: Uint8Array): boolean {
    if (!this.send(bytes)) return false;
    this.bytes += bytes.length;
    return true;
  }

  get heldFrames(): number {
    return this.held.length;
  }
}

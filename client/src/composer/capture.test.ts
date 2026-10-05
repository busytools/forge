/**
 * The frame side of a captured take: the bytes the wire carries and the
 * ring a take holds while the socket is down.
 *
 * The encoder is pinned against the same fixture the socket record carries
 * on the server side (`baselines/socket/4/frames.json`, `dictate_frame`),
 * so the two halves of the contract are the same vector read two ways.
 */

import { describe, expect, it } from 'vitest';

import {
  CODEC_PCM_I16,
  FRAME_SAMPLES,
  FrameChunker,
  FrameRing,
  RING_FRAMES,
  SAMPLE_RATE,
  encodeFrame,
} from './capture';

describe('the frame the wire carries', () => {
  it('is the codec tag then little-endian i16 at 2^15', () => {
    const bytes = encodeFrame([0, 0.5, -1, 0.25]);
    expect([...bytes]).toEqual([
      CODEC_PCM_I16,
      0x00,
      0x00, // 0
      0x00,
      0x40, // 16384 = 0.5 * 32768, low byte first
      0x00,
      0x80, // -32768, two's complement
      0x00,
      0x20, // 8192 = 0.25 * 32768
    ]);
  });

  it('clamps what the microphone cannot have meant', () => {
    // Full scale lands on the largest positive sample, not on 32768, which
    // does not fit; and an overshoot reads as full scale rather than
    // wrapping its sign.
    expect([...encodeFrame([1]).slice(1)]).toEqual([0xff, 0x7f]);
    expect([...encodeFrame([2]).slice(1)]).toEqual([0xff, 0x7f]);
    expect([...encodeFrame([-2]).slice(1)]).toEqual([0x00, 0x80]);
  });

  it('reads like the server decodes it', () => {
    // The server divides the i16 by 32768; a sample that survives the round
    // trip to within one step is the contract holding in both directions.
    for (const sample of [0, 0.1, -0.1, 0.5, -0.5, 0.999]) {
      const [, low, high] = encodeFrame([sample]);
      const value = ((high ?? 0) << 8) | (low ?? 0);
      const signed = value > 32767 ? value - 65536 : value;
      expect(Math.abs(signed / 32768 - sample)).toBeLessThan(1 / 32768);
    }
  });
});

describe('the 20 ms cadence', () => {
  it('composes whole frames out of whatever the worklet posts', () => {
    const chunker = new FrameChunker();
    expect(chunker.push(new Array(FRAME_SAMPLES / 2).fill(0))).toHaveLength(0);
    const frames = chunker.push(new Array(FRAME_SAMPLES / 2).fill(0));
    expect(frames).toHaveLength(1);
    expect(frames[0]?.length).toBe(1 + FRAME_SAMPLES * 2);
  });

  it('carries a remainder across pushes rather than dropping it', () => {
    const chunker = new FrameChunker();
    expect(chunker.push(new Array(FRAME_SAMPLES - 1).fill(0))).toHaveLength(0);
    const frames = chunker.push([0]);
    expect(frames, 'the sample held from the previous push completes the frame').toHaveLength(1);
  });

  it('emits the tail as it stands rather than padding it', () => {
    const chunker = new FrameChunker();
    chunker.push([0.5, 0.5, 0.5]);
    const tail = chunker.flush();
    expect(tail?.length, 'a short frame, not a padded one').toBe(1 + 3 * 2);
    expect(chunker.flush(), 'and nothing is left after it').toBeNull();
  });

  it('is a cadence the server reads without gaps', () => {
    // The meter polls a take-and-reset peak every 50 ms, so a chunk larger
    // than one poll would make alternate readings silence.
    expect((FRAME_SAMPLES / SAMPLE_RATE) * 1000).toBeLessThanOrEqual(50);
  });
});

describe('the ring a take holds while the socket is down', () => {
  /** A socket that only takes frames while `open`, recording what it took. */
  function socket(): { open: boolean; sent: Uint8Array[]; send: (bytes: Uint8Array) => boolean } {
    const held = { open: false, sent: [] as Uint8Array[] };
    return {
      get open() {
        return held.open;
      },
      set open(next: boolean) {
        held.open = next;
      },
      sent: held.sent,
      send(bytes: Uint8Array): boolean {
        if (!held.open) return false;
        held.sent.push(bytes);
        return true;
      },
    };
  }

  it('sends straight through while the socket takes frames', () => {
    const wire = socket();
    wire.open = true;
    const ring = new FrameRing(wire.send);
    ring.push(encodeFrame([0]));
    expect(wire.sent).toHaveLength(1);
    expect(ring.heldFrames).toBe(0);
  });

  it('holds while the socket is away, and flushes in the order spoken', () => {
    const wire = socket();
    const ring = new FrameRing(wire.send);
    ring.push(encodeFrame([0.1]));
    ring.push(encodeFrame([0.2]));
    expect(ring.heldFrames, 'both frames are held, not dropped').toBe(2);

    wire.open = true;
    expect(ring.flush()).toBe(true);
    expect(ring.heldFrames).toBe(0);
    expect(wire.sent).toEqual([encodeFrame([0.1]), encodeFrame([0.2])]);
  });

  it('drops the oldest frame when a take outgrows the ring', () => {
    const wire = socket();
    const ring = new FrameRing(wire.send, 2);
    for (const sample of [0.1, 0.2, 0.3]) ring.push(encodeFrame([sample]));
    expect(ring.heldFrames, 'the bound holds').toBe(2);

    wire.open = true;
    ring.flush();
    expect(wire.sent, 'the newest speech is what survives').toEqual([
      encodeFrame([0.2]),
      encodeFrame([0.3]),
    ]);
  });

  it('keeps its place when a flush lands on a socket that goes away again', () => {
    let budget = 0;
    const sent: Uint8Array[] = [];
    const ring = new FrameRing((bytes) => {
      if (budget <= 0) return false;
      budget -= 1;
      sent.push(bytes);
      return true;
    });
    ring.push(encodeFrame([0.1]));
    ring.push(encodeFrame([0.2]));

    budget = 1;
    expect(ring.flush(), 'the first frame goes, then the socket closes again').toBe(false);
    expect(ring.heldFrames, 'the frame that did not go is still here').toBe(1);

    budget = 2;
    expect(ring.flush()).toBe(true);
    expect(ring.heldFrames).toBe(0);
    expect(sent).toEqual([encodeFrame([0.1]), encodeFrame([0.2])]);
  });

  /**
   * The pair the row's wire line draws: frames PRODUCED and bytes the wire
   * TOOK. A frame held while the socket is down is the difference between
   * them, which is the whole diagnostic value of the two numbers.
   */
  it('counts what it produced and what the wire took', () => {
    let open = true;
    const ring = new FrameRing(() => open);
    ring.push(encodeFrame([0.1]));
    ring.push(encodeFrame([0.2]));
    expect(ring.frames, 'both frames were produced').toBe(2);
    expect(ring.bytes, 'and both went while the socket was up').toBe(6);

    open = false;
    ring.push(encodeFrame([0.3]));
    expect(ring.frames, 'the held frame is still produced').toBe(3);
    expect(ring.bytes, 'but the wire has not taken it').toBe(6);

    open = true;
    expect(ring.flush(), 'the flush carries it out').toBe(true);
    expect(ring.bytes, 'and then it counts').toBe(9);
  });

  it('bounds a take at about thirty seconds', () => {
    expect(RING_FRAMES * (FRAME_SAMPLES / SAMPLE_RATE)).toBeGreaterThanOrEqual(30);
  });
});

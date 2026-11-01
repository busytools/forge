/**
 * The hold's clip: the dictation encoder's frames, gathered as the one
 * utterance the wire carries, and cut at the hold's bound rather than
 * growing without end.
 */

import { describe, expect, it } from 'vitest';

import { HeldClip, MAX_HOLD_SECONDS, SAMPLE_RATE, frameSamples } from './capture.svelte';

/** One wire frame: the codec byte, then the samples little-endian. */
function frame(samples: number[]): Uint8Array {
  const bytes = new Uint8Array(1 + samples.length * 2);
  for (const [at, sample] of samples.entries()) {
    const value = sample & 0xffff;
    bytes[1 + at * 2] = value & 0xff;
    bytes[2 + at * 2] = (value >> 8) & 0xff;
  }
  return bytes;
}

describe('the held clip', () => {
  it('releases a held utterance as one clip', () => {
    const clip = new HeldClip();
    clip.push(frame([1, -2, 3]));
    clip.push(frame([4, -5]));
    expect(Array.from(clip.take())).toEqual([1, -2, 3, 4, -5]);
    expect(frameSamples(frame([1, -2]))).toEqual([1, -2]);
  });

  it('cuts a hold at the bound and says it was cut', () => {
    const clip = new HeldClip();
    const second = new Array<number>(SAMPLE_RATE).fill(1);
    for (let at = 0; at < MAX_HOLD_SECONDS + 1; at += 1) clip.push(frame(second));
    expect(clip.capped).toBe(true);
    expect(clip.take().length).toBe(MAX_HOLD_SECONDS * SAMPLE_RATE);
  });

  it('keeps a hold under the bound whole', () => {
    const clip = new HeldClip();
    clip.push(frame([1, 2, 3]));
    expect(clip.capped).toBe(false);
    expect(clip.take().length).toBe(3);
  });
});

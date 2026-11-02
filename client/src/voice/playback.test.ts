/**
 * The playback queue: a clip is scheduled the moment it arrives, behind the
 * ones before it, so a stream of short clips sounds continuous; a reconnect
 * throws every scheduled clip away rather than playing speech from a seat
 * nobody is watching any more.
 */

import { describe, expect, it } from 'vitest';

import { Playback, type Speaker } from './playback.svelte';

/** A speaker that records what it scheduled and finishes only when told to. */
class FakeSpeaker implements Speaker {
  played: { pcm: number[]; rate: number }[] = [];
  private readonly endings: (() => void)[] = [];
  stopped = 0;
  closed = false;

  async play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    await new Promise<void>((resolve) => this.endings.push(resolve));
  }

  stop(): void {
    this.stopped += 1;
    this.finishAll();
  }

  close(): void {
    this.closed = true;
  }

  /** Let the clip sounding now end. */
  finish(): void {
    this.endings.shift()?.();
  }

  /** Let every scheduled clip end, as a device stop does. */
  finishAll(): void {
    for (const ending of this.endings.splice(0)) ending();
  }
}

/** Let the player's own promise chain run. */
async function settle(): Promise<void> {
  for (let at = 0; at < 10; at += 1) await Promise.resolve();
}

describe('the playback queue', () => {
  it('schedules every chunk on arrival, in order, and clears at the last end', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);

    playback.push(Int16Array.from([1]), 24_000);
    await settle();
    expect(
      speaker.played.map((clip) => clip.pcm),
      'the first clip starts on arrival',
    ).toEqual([[1]]);
    expect(speaker.played[0]?.rate).toBe(24_000);

    playback.push(Int16Array.from([2]), 24_000);
    await settle();
    expect(
      speaker.played.map((clip) => clip.pcm),
      'the next clip is scheduled behind it rather than waiting for it to end',
    ).toEqual([[1], [2]]);
    expect(playback.playing, 'the switch is lit while any clip is queued').toBe(true);

    speaker.finish();
    await settle();
    expect(playback.playing, 'still one clip sounding').toBe(true);

    speaker.finish();
    await settle();
    expect(playback.playing).toBe(false);
  });

  it('drops every scheduled clip when the socket reconnects', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([1]), 24_000);
    playback.push(Int16Array.from([2]), 24_000);
    await settle();

    playback.drop();
    await settle();
    expect(speaker.stopped, 'the scheduled clips are stopped').toBe(1);
    expect(playback.playing, 'and the switch goes off with them').toBe(false);
  });

  it('stops the sounding clip when a drop is followed by a push', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([1]), 24_000);
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1]]);

    // The socket drops while the first clip is still sounding, then the
    // reconnected socket delivers a new one. The old clip is stopped, so the
    // new one is the only thing scheduled afterwards.
    playback.drop();
    playback.push(Int16Array.from([2]), 24_000);
    await settle();
    expect(speaker.stopped, 'the sounding clip is stopped').toBe(1);
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1], [2]]);
    expect(playback.playing, 'the new clip keeps the switch lit').toBe(true);

    speaker.finish();
    await settle();
    expect(playback.playing).toBe(false);
  });

  it('plays nothing for a clip that carries no samples', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([]), 24_000);
    await settle();
    expect(speaker.played).toEqual([]);
    expect(playback.playing).toBe(false);
  });
});

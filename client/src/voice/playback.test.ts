/**
 * The playback queue: a clip starts the moment it arrives, each later one
 * follows the moment the last ends, and a reconnect throws the queue away
 * rather than playing speech from a seat nobody is watching any more.
 */

import { describe, expect, it } from 'vitest';

import { Playback, type Speaker } from './playback.svelte';

/** A speaker that records what it played and finishes only when told to. */
class FakeSpeaker implements Speaker {
  played: { pcm: number[]; rate: number }[] = [];
  private readonly endings: (() => void)[] = [];
  closed = false;

  async play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    await new Promise<void>((resolve) => this.endings.push(resolve));
  }

  close(): void {
    this.closed = true;
  }

  /** Let the clip sounding now end. */
  finish(): void {
    this.endings.shift()?.();
  }
}

/** Let the player's own promise chain run. */
async function settle(): Promise<void> {
  for (let at = 0; at < 10; at += 1) await Promise.resolve();
}

describe('the playback queue', () => {
  it('starts playback on the first chunk and keeps the order', async () => {
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
    expect(speaker.played.length, 'the second clip waits for the first to end').toBe(1);

    speaker.finish();
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1], [2]]);
    expect(playback.playing).toBe(true);

    speaker.finish();
    await settle();
    expect(playback.playing).toBe(false);
  });

  it('drops the queue when the socket reconnects', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([1]), 24_000);
    playback.push(Int16Array.from([2]), 24_000);
    await settle();

    playback.drop();
    speaker.finish();
    await settle();
    expect(
      speaker.played.map((clip) => clip.pcm),
      'the queued clip never sounds',
    ).toEqual([[1]]);
    expect(playback.playing).toBe(false);
  });

  it('waits for a sounding clip when a drop is followed by a push', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([1]), 24_000);
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1]]);

    // The socket drops while the first clip is still sounding, then the
    // reconnected socket delivers a new one. The new clip must wait for the
    // speaker rather than start on top of the clip already playing.
    playback.drop();
    playback.push(Int16Array.from([2]), 24_000);
    await settle();
    expect(
      speaker.played.map((clip) => clip.pcm),
      'the new clip waits for the sounding one',
    ).toEqual([[1]]);

    speaker.finish();
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1], [2]]);

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

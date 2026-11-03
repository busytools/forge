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
  unlocked = 0;
  stopped = 0;
  closed = false;

  unlock(): Promise<void> {
    this.unlocked += 1;
    return Promise.resolve();
  }

  async play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    await new Promise<void>((resolve) => this.endings.push(resolve));
  }

  stop(): void {
    this.stopped += 1;
    this.finish();
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
  /** A browser holds a context created outside a gesture suspended, and a
   * clip scheduled on it is silent: the press wakes the device first. */
  it('wakes the audio device when a gesture asks it to', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);

    await playback.unlock();

    expect(speaker.unlocked).toBe(1);
  });

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
    expect(playback.started, 'the start is counted for a preview press').toBe(1);

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

  it('stops the sounding clip when a drop is followed by a push', async () => {
    const speaker = new FakeSpeaker();
    const playback = new Playback(speaker);
    playback.push(Int16Array.from([1]), 24_000);
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1]]);

    // The socket drops while the first clip is still sounding, then the
    // reconnected socket delivers a new one. The old clip is stopped, so the
    // new one never sounds on top of it.
    playback.drop();
    playback.push(Int16Array.from([2]), 24_000);
    await settle();
    expect(speaker.stopped, 'the sounding clip is stopped').toBe(1);
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

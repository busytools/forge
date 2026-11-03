/**
 * One seat's voice, over a fake port: the switch that follows the seat's
 * count, and the speech that plays only while this client is listening.
 */

import { describe, expect, it } from 'vitest';

import { SLOT } from '../composer/testing';
import type { SessionUpdate } from '../protocol';
import { Stores } from '../stores';
import type { Speaker } from './playback.svelte';
import { Voice, connectionPort, type VoicePort } from './voice.svelte';

class FakePort implements VoicePort {
  sent: { command: string; body: unknown }[] = [];
  queue: SessionUpdate[] = [];
  down = false;

  send(command: 'voice_mode', body: { on: boolean; explicit: boolean }): void {
    if (this.down) throw new Error('the socket is closed');
    this.sent.push({ command, body });
  }

  frames(): SessionUpdate[] {
    const out = this.queue;
    this.queue = [];
    return out;
  }

  seed(): number {
    return this.seeded;
  }

  seeded = 0;
}

class FakeSpeaker implements Speaker {
  played: { pcm: number[]; rate: number }[] = [];
  unlocked = 0;
  stopped = 0;
  closed = false;
  private readonly endings: (() => void)[] = [];

  unlock(): Promise<void> {
    this.unlocked += 1;
    return Promise.resolve();
  }

  play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    return new Promise<void>((resolve) => this.endings.push(resolve));
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

function voice(): { voice: Voice; port: FakePort; speaker: FakeSpeaker } {
  const port = new FakePort();
  const speaker = new FakeSpeaker();
  const instance = new Voice(port, speaker);
  return { voice: instance, port, speaker };
}

describe("one seat's voice", () => {
  it('forgets the switch and closes the speaker on a reset', () => {
    const at = voice();
    at.voice.turnOn();
    at.voice.reset();

    expect(at.voice.on).toBe(false);
    expect(at.voice.listeners).toBe(0);
    expect(at.speaker.closed).toBe(true);
  });

  /** Both gestures that ask for speech wake the device: the switch, whose
   * replies arrive much later, and the preview press. */
  it('wakes the audio device on the switch and on a preview press', () => {
    const at = voice();
    at.voice.turnOn();
    expect(at.speaker.unlocked).toBe(1);

    at.voice.unlock();
    expect(at.speaker.unlocked).toBe(2);
  });

  it("plays the seat's audio only while this client's voice is on", () => {
    const at = voice();
    at.port.queue.push({ voice_audio: { key: {}, seq: 0, sample_rate: 24_000, pcm: [1, 2] } });
    at.voice.pump();
    expect(at.speaker.played, 'a client that is not listening stays silent').toEqual([]);

    at.voice.turnOn();
    at.port.queue.push({ voice_audio: { key: {}, seq: 1, sample_rate: 24_000, pcm: [3, 4] } });
    at.voice.pump();
    expect(at.speaker.played).toEqual([{ pcm: [3, 4], rate: 24_000 }]);
  });

  /** A preview press takes the sounding clip off the speaker first, so the
   * pressed voice is heard as its first chunk arrives rather than queued
   * behind a clip already playing. */
  it('stops the sounding clip when a preview press arrives, and the next plays', async () => {
    const at = voice();
    at.voice.turnOn();
    at.port.queue.push({ voice_audio: { key: {}, seq: 0, sample_rate: 24_000, pcm: [1] } });
    at.voice.pump();
    expect(at.speaker.played.map((clip) => clip.pcm)).toEqual([[1]]);

    at.voice.stop();
    expect(at.speaker.stopped, 'the old preview stopped').toBe(1);

    at.port.queue.push({ voice_audio: { key: {}, seq: 1, sample_rate: 24_000, pcm: [2] } });
    at.voice.pump();
    await settle();
    expect(
      at.speaker.played.map((clip) => clip.pcm),
      'the pressed voice starts as soon as its chunk is there',
    ).toEqual([[1], [2]]);
  });

  it("stops the old occupant's speech when the seat is replaced", () => {
    const at = voice();
    at.voice.turnOn();
    at.port.queue.push({ voice_audio: { key: {}, seq: 0, sample_rate: 24_000, pcm: [1] } });
    at.voice.pump();
    expect(at.speaker.played.length).toBe(1);

    at.port.queue.push({ session_replaced: { key: {} } });
    at.voice.pump();

    expect(at.speaker.stopped, 'the old occupant is not speaking any more').toBe(1);
  });

  it('seeds the room from the seat read', () => {
    const port = new FakePort();
    port.seeded = 2;
    const instance = new Voice(port);

    expect(instance.listeners, 'a late attach reads the room rather than zero').toBe(2);
  });

  it('turns the switch off and stops playback when the seat reports no listeners', () => {
    const at = voice();
    at.voice.turnOn();
    at.port.queue.push({ voice_audio: { key: {}, seq: 0, sample_rate: 24_000, pcm: [1] } });
    at.voice.pump();
    expect(at.speaker.played.length).toBe(1);

    at.port.queue.push({ voice_mode_state: { key: {}, listeners: 0 } });
    at.voice.pump();

    expect(at.voice.on).toBe(false);
    expect(at.speaker.stopped, 'the queued speech goes with the switch').toBe(1);
  });

  it('seeds at the end of a store that already holds audio', () => {
    const stores = new Stores();
    const store = stores.open({ session: SLOT });
    store.push({ voice_audio: { key: SLOT, seq: 0, sample_rate: 24_000, pcm: [1, 2] } });
    const port = connectionPort(
      { store: (subject) => stores.get(subject), dispatch: () => Promise.resolve(null) },
      SLOT,
    );

    expect(port.frames(), 'the first read replays nothing').toEqual([]);
    store.push({ voice_audio: { key: SLOT, seq: 1, sample_rate: 24_000, pcm: [3, 4] } });
    const fresh = port.frames();
    expect(fresh).toHaveLength(1);
    expect((fresh[0] as { voice_audio: { pcm: number[] } }).voice_audio.pcm).toEqual([3, 4]);
    expect(
      store.updates().filter((update) => typeof update === 'object' && 'voice_audio' in update),
      'consumed audio does not stay in the retained tail',
    ).toEqual([]);
  });

  it('resubscribes after a reconnect when the client still wants voice', () => {
    const at = voice();
    at.voice.turnOn();
    at.voice.reset();
    at.voice.resume();

    expect(at.voice.on).toBe(true);
    expect(at.port.sent.map((sent) => sent.command)).toEqual(['voice_mode', 'voice_mode']);
  });

  it('turns the switch off when the socket is down', () => {
    const at = voice();
    at.port.down = true;
    at.voice.turnOn();

    expect(at.voice.on).toBe(false);
    expect(at.voice.line).toContain('socket is down');

    at.port.down = false;
    at.voice.resume();
    expect(at.voice.on, 'a click that never reached the server is not remembered').toBe(false);
  });

  it("pauses the seat's voice when the reader moves, and resumes on return", () => {
    const at = voice();
    at.voice.turnOn();
    at.voice.pause();

    expect(at.voice.on).toBe(false);
    expect(at.port.sent.at(-1)?.body).toEqual({ on: false, explicit: false });

    at.voice.resume();
    expect(at.voice.on).toBe(true);
    expect(at.port.sent.at(-1)?.body).toEqual({ on: true, explicit: false });
  });
});

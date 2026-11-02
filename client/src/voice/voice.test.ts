/**
 * One seat's voice, over a fake port and a fake microphone: the hold that
 * becomes an utterance, the switch that follows the seat's count, and the
 * speech that plays only while this client is listening.
 */

import { describe, expect, it } from 'vitest';

import type { SessionUpdate } from '../protocol';
import { HeldClip, type VoiceCapture } from './capture.svelte';
import type { Speaker } from './playback.svelte';
import { Voice, type VoicePort } from './voice.svelte';

class FakePort implements VoicePort {
  sent: { command: string; body: unknown }[] = [];
  queue: SessionUpdate[] = [];
  down = false;

  send(command: 'voice_mode', body: { on: boolean }): void;
  send(command: 'voice_utterance', body: { pcm: number[] }): void;
  send(command: 'voice_mode' | 'voice_utterance', body: { on: boolean } | { pcm: number[] }): void {
    if (this.down) throw new Error('the socket is closed');
    this.sent.push({ command, body });
  }

  frames(): SessionUpdate[] {
    const out = this.queue;
    this.queue = [];
    return out;
  }
}

class FakeCapture {
  clip = new HeldClip();
  cancelled = false;

  release(): Promise<HeldClip> {
    return Promise.resolve(this.clip);
  }

  cancel(): void {
    this.cancelled = true;
  }
}

class FakeSpeaker implements Speaker {
  played: { pcm: number[]; rate: number }[] = [];
  stopped = 0;
  closed = false;

  play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    return Promise.resolve();
  }

  stop(): void {
    this.stopped += 1;
  }

  close(): void {
    this.closed = true;
  }
}

function voice(open?: () => Promise<VoiceCapture>): {
  voice: Voice;
  port: FakePort;
  speaker: FakeSpeaker;
} {
  const port = new FakePort();
  const speaker = new FakeSpeaker();
  const instance = new Voice(
    port,
    open ?? (() => Promise.resolve(new FakeCapture() as unknown as VoiceCapture)),
    speaker,
  );
  return { voice: instance, port, speaker };
}

/** One 20 ms frame: a codec byte, then little-endian i16 samples. */
function frame(...samples: number[]): Uint8Array {
  const bytes = new Uint8Array(1 + samples.length * 2);
  samples.forEach((sample, at) => {
    bytes[1 + at * 2] = sample & 0xff;
    bytes[2 + at * 2] = (sample >> 8) & 0xff;
  });
  return bytes;
}

describe("one seat's voice", () => {
  it('sends the whole clip as one utterance on release', async () => {
    const at = voice();
    at.voice.turnOn();
    await at.voice.startTalk();
    at.voice.clip?.push(frame(1, 2));
    await at.voice.endTalk();

    expect(at.port.sent.map((sent) => sent.command)).toEqual(['voice_mode', 'voice_utterance']);
    expect(at.port.sent.at(-1)?.body).toEqual({ pcm: [1, 2] });
  });

  it('sends nothing for a hold that said nothing', async () => {
    const at = voice();
    at.voice.turnOn();
    await at.voice.startTalk();
    at.voice.cancelTalk();
    at.voice.turnOff();

    expect(at.port.sent.map((sent) => sent.command)).toEqual(['voice_mode', 'voice_mode']);
    expect(at.port.sent.at(-1)?.body).toEqual({ on: false });
  });

  it('draws the reason a microphone would not open', async () => {
    const at = voice(() => Promise.reject(new DOMException('denied', 'NotAllowedError')));
    at.voice.turnOn();
    await at.voice.startTalk();

    expect(at.voice.line).toContain('refused');
    expect(at.voice.state).toBe('listening');
  });

  it('says when a hold ran past its bound', async () => {
    const at = voice();
    at.voice.turnOn();
    await at.voice.startTalk();
    const clip = at.voice.clip;
    if (clip === null) throw new Error('the hold did not open');
    clip.capped = true;
    clip.push(frame(1));
    await at.voice.endTalk();

    expect(at.voice.line).toContain('60s');
  });

  it('forgets the switch and closes the speaker on a reset', () => {
    const at = voice();
    at.voice.turnOn();
    at.voice.reset();

    expect(at.voice.on).toBe(false);
    expect(at.voice.listeners).toBe(0);
    expect(at.speaker.closed).toBe(true);
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

  it('turns the switch off when the seat reports no listeners', () => {
    const at = voice();
    at.voice.turnOn();
    at.port.queue.push({ voice_mode_state: { key: {}, listeners: 0 } });
    at.voice.pump();

    expect(at.voice.on).toBe(false);
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
  });

  it("turns the seat's voice off when the control leaves it", () => {
    const at = voice();
    at.voice.turnOn();
    at.voice.detach();

    expect(at.port.sent.at(-1)?.body).toEqual({ on: false });
  });
});

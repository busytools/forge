/**
 * One seat's voice: the switch, the hold that becomes an utterance, and the
 * speech coming back. A fake port and a fake capture stand in for the socket
 * and the microphone, so each transition is pinned by name.
 */

import { describe, expect, it } from 'vitest';

import { SLOT } from '../composer/testing';
import type { SessionUpdate } from '../protocol';
import { HeldClip, type VoiceCapture } from './capture.svelte';
import type { Speaker } from './playback.svelte';
import { Voice, connectionPort, micFailure, type VoicePort } from './voice.svelte';

/** A port that records every command and hands back the frames a test queued. */
class FakePort implements VoicePort {
  sent: { command: string; body: unknown }[] = [];
  private queued: SessionUpdate[] = [];

  send(command: 'voice_mode', body: { on: boolean }): void;
  send(command: 'voice_utterance', body: { pcm: number[] }): void;
  send(command: string, body: unknown): void {
    this.sent.push({ command, body });
  }

  frames(): SessionUpdate[] {
    const held = this.queued;
    this.queued = [];
    return held;
  }

  /** Queue one frame the core would send. */
  push(update: SessionUpdate): void {
    this.queued.push(update);
  }
}

/** A capture whose release and cancel a test can read. */
class FakeCapture {
  clip = new HeldClip();
  released = false;
  cancelled = false;

  async release(): Promise<HeldClip> {
    this.released = true;
    return this.clip;
  }

  cancel(): void {
    this.cancelled = true;
  }
}

/** A speaker that records what it played and finishes only when told to. */
class FakeSpeaker implements Speaker {
  played: { pcm: number[]; rate: number }[] = [];
  private readonly endings: (() => void)[] = [];

  async play(pcm: Int16Array, rate: number): Promise<void> {
    this.played.push({ pcm: Array.from(pcm), rate });
    await new Promise<void>((resolve) => this.endings.push(resolve));
  }

  close(): void {}

  /** Let the clip sounding now end. */
  finish(): void {
    this.endings.shift()?.();
  }
}

/** Let the player's own promise chain run. */
async function settle(): Promise<void> {
  for (let at = 0; at < 10; at += 1) await Promise.resolve();
}

/** A connection whose seat store a test fills by hand. */
class FakeConnection {
  held: SessionUpdate[] = [];
  lost = 0;
  sent: { command: Record<string, unknown> }[] = [];

  store(_what: unknown): {
    updates: () => SessionUpdate[];
    dropped: () => number;
  } {
    return { updates: () => this.held, dropped: () => this.lost };
  }

  dispatch(command: Record<string, unknown>): Promise<null> {
    this.sent.push({ command });
    return Promise.resolve(null);
  }
}

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

/** A voice over a port and a speaker whose capture a test supplies. */
function makeVoice(): { voice: Voice; port: FakePort; speaker: FakeSpeaker } {
  const port = new FakePort();
  const speaker = new FakeSpeaker();
  const open = async () => new FakeCapture() as unknown as VoiceCapture;
  return { voice: new Voice(port, open, speaker), port, speaker };
}

/** A voice whose open a test resolves by hand. */
function makeOpening(): {
  voice: Voice;
  port: FakePort;
  speaker: FakeSpeaker;
  open: () => FakeCapture;
  arrive: () => Promise<FakeCapture>;
} {
  const port = new FakePort();
  const speaker = new FakeSpeaker();
  let resolve: ((capture: FakeCapture) => void) | null = null;
  const open = () => new FakeCapture();
  const voice = new Voice(
    port,
    () =>
      new Promise((done) => {
        resolve = (capture) => done(capture as never);
      }),
    speaker,
  );
  return {
    voice,
    port,
    speaker,
    open,
    arrive: async () => {
      const capture = open();
      resolve?.(capture);
      await settle();
      return capture;
    },
  };
}

describe('the voice hold', () => {
  it('sends the held clip as one utterance on release', async () => {
    const { voice, port } = makeVoice();
    voice.turnOn();
    await voice.startTalk();
    voice.clip?.push(frame([1, -2, 3]));
    await voice.endTalk();
    const utterance = port.sent.find((sent) => sent.command === 'voice_utterance');
    expect(utterance?.body).toEqual({ pcm: [1, -2, 3] });
    expect(voice.state).toBe('listening');
  });

  it('sends nothing for a tap that said nothing', async () => {
    const { voice, port } = makeVoice();
    voice.turnOn();
    await voice.startTalk();
    await voice.endTalk();
    expect(port.sent.some((sent) => sent.command === 'voice_utterance')).toBe(false);
  });

  it('cancels the capture and sends nothing', async () => {
    const { voice, port } = makeVoice();
    voice.turnOn();
    await voice.startTalk();
    const capture = (voice as unknown as { capture: FakeCapture }).capture;
    voice.cancelTalk();
    expect(capture.cancelled).toBe(true);
    expect(port.sent.some((sent) => sent.command === 'voice_utterance')).toBe(false);
  });

  it('says a capped hold was cut at the bound', async () => {
    const { voice } = makeVoice();
    voice.turnOn();
    await voice.startTalk();
    voice.clip?.push(frame([1]));
    (voice.clip as HeldClip).capped = true;
    await voice.endTalk();
    expect(voice.line).toContain('ran past 60s');
  });

  it('reports a microphone that will not open', async () => {
    const port = new FakePort();
    const voice = new Voice(
      port,
      async () => {
        throw new DOMException('denied', 'NotAllowedError');
      },
      new FakeSpeaker(),
    );
    voice.turnOn();
    await voice.startTalk();
    expect(voice.line).toBe(micFailure(new DOMException('denied', 'NotAllowedError')));
    expect(voice.state).toBe('listening');
  });

  it('keeps a failure silent when the hold was already released', async () => {
    const port = new FakePort();
    const pending: { reject: ((why: unknown) => void) | null } = { reject: null };
    const voice = new Voice(
      port,
      () =>
        new Promise((_done, fail) => {
          pending.reject = fail;
        }),
      new FakeSpeaker(),
    );
    voice.turnOn();
    const opening = voice.startTalk();
    await voice.endTalk();
    pending.reject?.(new DOMException('denied', 'NotAllowedError'));
    await opening;
    expect(voice.line).toBeNull();
  });
});

describe('the open that outlives its hold', () => {
  it('closes the device when the release lands while opening', async () => {
    const { voice, port, arrive } = makeOpening();
    voice.turnOn();
    const opening = voice.startTalk();
    await voice.endTalk();
    const capture = await arrive();
    await opening;
    expect(capture.cancelled).toBe(true);
    expect((voice as unknown as { capture: FakeCapture | null }).capture).toBeNull();
    expect(port.sent.some((sent) => sent.command === 'voice_utterance')).toBe(false);
    expect(voice.state).toBe('listening');
  });

  it('closes the device when the cancel lands while opening', async () => {
    const { voice, arrive } = makeOpening();
    voice.turnOn();
    const opening = voice.startTalk();
    voice.cancelTalk();
    const capture = await arrive();
    await opening;
    expect(capture.cancelled).toBe(true);
    expect((voice as unknown as { capture: FakeCapture | null }).capture).toBeNull();
  });

  it('closes the device when the seat goes while opening', async () => {
    const { voice, arrive } = makeOpening();
    voice.turnOn();
    const opening = voice.startTalk();
    voice.detach();
    const capture = await arrive();
    await opening;
    expect(capture.cancelled).toBe(true);
    expect((voice as unknown as { capture: FakeCapture | null }).capture).toBeNull();
  });

  it('does not open a second microphone for a second press', async () => {
    const { voice, arrive } = makeOpening();
    voice.turnOn();
    const first = voice.startTalk();
    await voice.startTalk();
    const capture = await arrive();
    await first;
    expect(capture.cancelled).toBe(false);
    expect((voice as unknown as { capture: FakeCapture | null }).capture).not.toBeNull();
  });
});

describe('the seat frames', () => {
  it('pumps the seat audio into playback', async () => {
    const { voice, port, speaker } = makeVoice();
    voice.turnOn();
    port.push({
      voice_audio: { seq: 1, sample_rate: 24_000, pcm: [1, 2, 3] },
    } as unknown as SessionUpdate);
    voice.pump();
    await settle();
    expect(speaker.played.map((clip) => clip.pcm)).toEqual([[1, 2, 3]]);
    expect(speaker.played[0]?.rate).toBe(24_000);
  });

  it('plays nothing while this client has voice off', async () => {
    const { voice, port, speaker } = makeVoice();
    port.push({
      voice_audio: { seq: 1, sample_rate: 24_000, pcm: [4, 5, 6] },
    } as unknown as SessionUpdate);
    voice.pump();
    await settle();
    expect(speaker.played).toEqual([]);
    expect(voice.state).toBe('off');
  });

  it('follows the listener count the core reports', () => {
    const { voice, port } = makeVoice();
    voice.turnOn();
    expect(voice.on).toBe(true);
    port.push({ voice_mode_state: { listeners: 2 } } as unknown as SessionUpdate);
    voice.pump();
    expect(voice.listeners).toBe(2);
    expect(voice.on).toBe(true);
    port.push({ voice_mode_state: { listeners: 0 } } as unknown as SessionUpdate);
    voice.pump();
    expect(voice.listeners).toBe(0);
    expect(voice.on).toBe(false);
  });

  it('drops the switch and the queue on reset', async () => {
    const { voice, port, speaker } = makeVoice();
    voice.turnOn();
    await voice.startTalk();
    port.push({
      voice_audio: { seq: 1, sample_rate: 24_000, pcm: [1] },
    } as unknown as SessionUpdate);
    voice.pump();
    await settle();
    expect(speaker.played.length).toBe(1);
    voice.reset();
    expect(voice.on).toBe(false);
    expect(voice.listeners).toBe(0);
    expect(voice.line).toBeNull();
    expect(voice.state).toBe('off');
    expect(port.sent.some((sent) => sent.command === 'voice_mode')).toBe(true);
  });
});

describe('the connection port', () => {
  it('seeds past the audio a seat already retained', () => {
    const connection = new FakeConnection();
    connection.held.push({
      voice_audio: { seq: 0, sample_rate: 24_000, pcm: [9] },
    } as unknown as SessionUpdate);
    const port = connectionPort(connection as never, SLOT);
    expect(port.frames(), 'retained audio is not replayed on a new port').toEqual([]);
    const fresh = {
      voice_audio: { seq: 1, sample_rate: 24_000, pcm: [1] },
    } as unknown as SessionUpdate;
    connection.held.push(fresh);
    expect(port.frames()).toEqual([fresh]);
  });

  it('sends commands under the seat key', () => {
    const connection = new FakeConnection();
    const port = connectionPort(connection as never, SLOT);
    port.send('voice_mode', { on: true });
    expect(connection.sent).toEqual([{ command: { voice_mode: { key: SLOT, on: true } } }]);
  });
});

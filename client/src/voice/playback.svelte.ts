/**
 * The seat's speech, played in the order the engine rendered it.
 *
 * The engine renders one sentence at a time and each arrives as its own
 * clip, so a queue is what keeps a sentence from stepping on the one before
 * it: playback starts on the first clip and each later one follows the
 * moment it ends. Nothing mixes and nothing skips.
 *
 * **A reconnected socket drops the queue.** Frames missed while the socket
 * was down are frames nobody will resend, so a queue carried across a
 * reconnect would play a conversation with holes in it, behind everything
 * the reader is hearing now.
 */

/** Where decoded clips go. An interface so a test needs no audio stack. */
export interface Speaker {
  /** Play one clip, resolving when it has finished sounding. */
  play(pcm: Int16Array, sampleRate: number): Promise<void>;
  /** Release the device the speaker was using. */
  close(): void;
}

/**
 * The browser's own speaker: one `AudioContext`, one buffer per clip, played
 * in turn. Created on the first clip, because a context built before any
 * interaction starts suspended on most platforms.
 */
export function webSpeaker(): Speaker {
  let context: AudioContext | null = null;
  return {
    async play(pcm: Int16Array, sampleRate: number): Promise<void> {
      const audio = context ?? new AudioContext({ sampleRate });
      context = audio;
      const buffer = audio.createBuffer(1, Math.max(pcm.length, 1), sampleRate);
      const channel = buffer.getChannelData(0);
      for (let at = 0; at < pcm.length; at += 1) channel[at] = (pcm[at] ?? 0) / 32768;
      const source = audio.createBufferSource();
      source.buffer = buffer;
      source.connect(audio.destination);
      await new Promise<void>((resolve) => {
        source.onended = () => resolve();
        source.start();
      });
    },
    close(): void {
      void context?.close();
      context = null;
    },
  };
}

export class Playback {
  private queue: { pcm: Int16Array; sampleRate: number }[] = [];
  /**
   * Bumped by every drop, so a clip sounding while the queue is dropped does
   * not go on to start the next one.
   */
  private generation = 0;
  /** Whether a clip is sounding now. */
  playing = $state(false);

  constructor(private readonly speaker: Speaker = webSpeaker()) {}

  /** Add one clip and start playing when nothing is. */
  push(pcm: Int16Array, sampleRate: number): void {
    if (pcm.length === 0) return;
    this.queue.push({ pcm, sampleRate });
    void this.pump();
  }

  /** Discard everything queued, and whatever is sounding, with it. */
  drop(): void {
    this.generation += 1;
    this.queue = [];
    this.playing = false;
  }

  /**
   * Play queued clips until the queue is empty or the generation moves.
   *
   * The loop is the queue's one player: every push starts it and finds it
   * already running, so two clips can never sound at once.
   */
  private async pump(): Promise<void> {
    if (this.playing) return;
    const generation = this.generation;
    this.playing = true;
    while (this.queue.length > 0 && generation === this.generation) {
      const next = this.queue.shift();
      if (next === undefined) break;
      await this.speaker.play(next.pcm, next.sampleRate);
    }
    if (generation === this.generation) this.playing = false;
  }
}

/**
 * The seat's speech, played in the order the engine rendered it: each clip
 * scheduled behind the one before, so a stream of short clips sounds as one
 * voice rather than a stutter.
 *
 * A reconnected socket drops the queue, because frames missed while it was
 * down are frames nobody will resend.
 */

/** Where decoded clips go. An interface so a test needs no audio stack. */
export interface Speaker {
  /** Schedule one clip behind what is already sounding, resolving when it ends. */
  play(pcm: Int16Array, sampleRate: number): Promise<void>;
  /** Stop every scheduled clip now. */
  stop(): void;
  /** Release the device the speaker was using. */
  close(): void;
}

/**
 * The browser's own speaker: one `AudioContext`, one buffer per clip, laid
 * on its timeline back to back. Created on the first clip, because a context
 * built before any interaction starts suspended on most platforms.
 */
export function webSpeaker(): Speaker {
  let context: AudioContext | null = null;
  const sounding: AudioBufferSourceNode[] = [];
  /** When the queued clips stop sounding, on the context's own clock. */
  let nextStart = 0;
  return {
    async play(pcm: Int16Array, sampleRate: number): Promise<void> {
      const audio = context ?? new AudioContext({ sampleRate });
      context = audio;
      // A context built outside a gesture starts suspended: without this the
      // source schedules silently and `onended` never fires.
      void audio.resume();
      const buffer = audio.createBuffer(1, Math.max(pcm.length, 1), sampleRate);
      const channel = buffer.getChannelData(0);
      for (let at = 0; at < pcm.length; at += 1) channel[at] = (pcm[at] ?? 0) / 32768;
      const source = audio.createBufferSource();
      source.buffer = buffer;
      source.connect(audio.destination);
      // **Behind what is queued, never at the clock**: a start in the past
      // sounds late, which is a gap, so the cushion holds the first clip off
      // the clock and every later one takes the previous clip's end.
      const at = Math.max(audio.currentTime + 0.02, nextStart);
      nextStart = at + buffer.duration;
      sounding.push(source);
      await new Promise<void>((resolve) => {
        source.onended = () => {
          const held = sounding.indexOf(source);
          if (held >= 0) sounding.splice(held, 1);
          resolve();
        };
        source.start(at);
      });
    },
    stop(): void {
      for (const source of sounding) source.stop();
      sounding.length = 0;
      nextStart = 0;
    },
    close(): void {
      for (const source of sounding) source.stop();
      sounding.length = 0;
      nextStart = 0;
      void context?.close();
      context = null;
    },
  };
}

export class Playback {
  private queue: { pcm: Int16Array; sampleRate: number }[] = [];
  /** Whether a clip is sounding now, or queued on the speaker's timeline. */
  playing = $state(false);
  /** Clips scheduled and not yet ended. */
  private sounding = 0;
  /** Bumped by a drop, so a stopped clip's ending does not count. */
  private generation = 0;

  constructor(private readonly speaker: Speaker = webSpeaker()) {}

  /** Add one clip and schedule it behind the ones already queued. */
  push(pcm: Int16Array, sampleRate: number): void {
    if (pcm.length === 0) return;
    this.queue.push({ pcm, sampleRate });
    this.drain();
  }

  /// Discard everything queued and stop every clip now; `playing` goes false
  /// with the clips, and their endings are ignored.
  drop(): void {
    this.queue = [];
    this.generation += 1;
    this.sounding = 0;
    this.playing = false;
    this.speaker.stop();
  }

  /** Release the audio device. */
  close(): void {
    this.drop();
    this.speaker.close();
  }

  /**
   * Schedule every queued clip without waiting for the ones before it.
   *
   * **A streamed clip arrives while the last is still sounding**, so waiting
   * for its end before scheduling would put a gap at every chunk boundary.
   * The speaker lays each one after the previous on its own clock, which is
   * what makes a run of short clips one continuous voice.
   */
  private drain(): void {
    const generation = this.generation;
    while (this.queue.length > 0) {
      const next = this.queue.shift();
      if (next === undefined) break;
      this.sounding += 1;
      this.playing = true;
      void this.speaker.play(next.pcm, next.sampleRate).finally(() => {
        if (generation !== this.generation) return;
        this.sounding = Math.max(0, this.sounding - 1);
        if (this.sounding === 0) this.playing = false;
      });
    }
  }
}

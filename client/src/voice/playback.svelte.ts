/**
 * The seat's speech, played in the order the engine rendered it: one clip
 * at a time, each following the one before.
 *
 * A reconnected socket drops the queue, because frames missed while it was
 * down are frames nobody will resend.
 */

/** Where decoded clips go. An interface so a test needs no audio stack. */
export interface Speaker {
  /** Play one clip, resolving when it has finished sounding. */
  play(pcm: Int16Array, sampleRate: number): Promise<void>;
  /** Stop the clip sounding now, if one is. */
  stop(): void;
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
  let sounding: AudioBufferSourceNode | null = null;
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
      sounding = source;
      await new Promise<void>((resolve) => {
        source.onended = () => {
          if (sounding === source) sounding = null;
          resolve();
        };
        source.start();
      });
    },
    stop(): void {
      sounding?.stop();
      sounding = null;
    },
    close(): void {
      void context?.close();
      context = null;
    },
  };
}

export class Playback {
  private queue: { pcm: Int16Array; sampleRate: number }[] = [];
  /** Whether a clip is sounding now. */
  playing = $state(false);

  constructor(private readonly speaker: Speaker = webSpeaker()) {}

  /** Add one clip and start playing when nothing is. */
  push(pcm: Int16Array, sampleRate: number): void {
    if (pcm.length === 0) return;
    this.queue.push({ pcm, sampleRate });
    void this.pump();
  }

  /**
   * Discard everything queued and stop the clip sounding now. `playing`
   * stays true until the stopped clip's own promise resolves, so a push
   * during it waits rather than starting a second clip on top of the first.
   */
  drop(): void {
    this.queue = [];
    this.speaker.stop();
  }

  /** Release the audio device. */
  close(): void {
    this.queue = [];
    this.speaker.stop();
    this.speaker.close();
  }

  /**
   * Play queued clips until the queue is empty.
   *
   * The loop is the queue's one player: every push starts it and finds it
   * already running, so two clips can never sound at once. A drop while a
   * clip sounds clears the queue but leaves that clip to finish; a push
   * during it joins the new queue and waits for the speaker rather than
   * starting a second clip over the first.
   */
  private async pump(): Promise<void> {
    if (this.playing) return;
    this.playing = true;
    try {
      while (this.queue.length > 0) {
        const next = this.queue.shift();
        if (next === undefined) break;
        await this.speaker.play(next.pcm, next.sampleRate);
      }
    } catch {
      // A clip the device refused is dropped; the next push starts a pump.
    } finally {
      this.playing = false;
    }
  }
}

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
   * Discard everything queued. A clip already sounding cannot be stopped
   * through the speaker seam, so it is left to end on its own; `playing`
   * stays true while it does, or a push during it would start a second clip
   * on top of the first.
   */
  drop(): void {
    this.queue = [];
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
    while (this.queue.length > 0) {
      const next = this.queue.shift();
      if (next === undefined) break;
      await this.speaker.play(next.pcm, next.sampleRate);
    }
    this.playing = false;
  }
}

/**
 * The seat's speech, played in the order the engine rendered it: one clip
 * at a time, each following the one before.
 *
 * A reconnected socket drops the queue, because frames missed while it was
 * down are frames nobody will resend.
 */

/** Where decoded clips go. An interface so a test needs no audio stack. */
export interface Speaker {
  /**
   * Create or resume the audio device. Called from the press's own handler,
   * because a browser holds a context created outside a gesture suspended:
   * a clip played on a suspended context starts and is silent.
   */
  unlock(): Promise<void>;
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
  /** The one context, resumed if the platform holds it suspended. `null` on
   * a page with no AudioContext at all (a test's jsdom), which has nothing to
   * wake and nothing to play through. */
  const device = async (): Promise<AudioContext | null> => {
    if (typeof AudioContext === 'undefined') return null;
    const audio = context ?? new AudioContext();
    context = audio;
    if (audio.state !== 'running') await audio.resume();
    return audio;
  };
  return {
    async unlock(): Promise<void> {
      await device();
    },
    async play(pcm: Int16Array, sampleRate: number): Promise<void> {
      // A context built outside a gesture starts suspended: without the
      // resume the source schedules silently and `onended` never fires,
      // stalling the queue behind it. The press resumes it first; this is
      // the backstop for a clip nobody unlocked for.
      const audio = await device();
      if (audio === null) return;
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
  /** Clips that have begun sounding, ever. A preview press records this, so
   * the clip it stopped cannot mark the new press as playing. */
  started = $state(0);

  constructor(private readonly speaker: Speaker = webSpeaker()) {}

  /** Create or resume the audio device from a user gesture, so the clip that
   * gesture asks for is audible rather than silent on a suspended context.
   * A device that refuses the wake is silence, not a failure to report. */
  async unlock(): Promise<void> {
    try {
      await this.speaker.unlock();
    } catch {
      // Nothing to play through; the push path reports what it cannot play.
    }
  }

  /** Add one clip and start playing when nothing is. */
  push(pcm: Int16Array, sampleRate: number): void {
    if (pcm.length === 0) return;
    this.queue.push({ pcm, sampleRate });
    void this.pump();
  }

  /// Discard everything queued and stop the clip sounding now; `playing`
  /// stays true until that clip's own promise resolves.
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

  /// Play queued clips until the queue is empty. Every push starts this
  /// loop and finds it already running, so two clips never sound at once.
  private async pump(): Promise<void> {
    if (this.playing) return;
    this.playing = true;
    try {
      while (this.queue.length > 0) {
        const next = this.queue.shift();
        if (next === undefined) break;
        this.started += 1;
        await this.speaker.play(next.pcm, next.sampleRate);
      }
    } catch {
      // A clip the device refused is dropped; the next push starts a pump.
    } finally {
      this.playing = false;
    }
  }
}

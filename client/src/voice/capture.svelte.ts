/**
 * The microphone side of a hold-to-talk clip: the frames a source produces,
 * gathered as the one complete clip the wire carries.
 *
 * The frames are the dictation encoder's own - `openCapture` hands back the
 * same 20 ms, one header byte then little-endian i16, that a take streams -
 * so this decodes them rather than encoding anything a second time. What it
 * does NOT do is hold a ring or a level: a hold is one clip released whole,
 * not a stream, so a frame that fails to arrive is a gap rather than
 * something to catch up on.
 */

import { isTauri } from '@tauri-apps/api/core';

import { SAMPLE_RATE } from '../composer/capture.svelte';
import { openCapture } from '../composer/dictate.svelte';
import { FLUSH_SETTLE_MS, type MicSource } from '../composer/take';

export { SAMPLE_RATE };

/** How long one hold may run before the control ends it. */
export const MAX_HOLD_SECONDS = 60;

/** The samples in one wire frame: one codec byte, then i16 little-endian. */
export function frameSamples(bytes: Uint8Array): number[] {
  const samples: number[] = [];
  for (let at = 1; at + 1 < bytes.length; at += 2) {
    const lo = bytes[at] ?? 0;
    const hi = bytes[at + 1] ?? 0;
    samples.push(((lo | (hi << 8)) << 16) >> 16);
  }
  return samples;
}

/**
 * One hold's audio, gathered until release.
 *
 * **The cap truncates rather than refuses.** A hold that runs past the bound
 * is the reader still talking; the part already said is the part worth
 * sending, and `capped` is what the control draws beside it.
 */
export class HeldClip {
  private held: number[] = [];
  /** Frames the source handed over, which the control's ticker reads. */
  frames = $state(0);
  /** Whether the hold ran past [`MAX_HOLD_SECONDS`] and was cut there. */
  capped = $state(false);

  get seconds(): number {
    return this.held.length / SAMPLE_RATE;
  }

  push(bytes: Uint8Array): void {
    if (this.capped) return;
    this.frames += 1;
    for (const sample of frameSamples(bytes)) {
      if (this.held.length >= MAX_HOLD_SECONDS * SAMPLE_RATE) {
        this.capped = true;
        return;
      }
      this.held.push(sample);
    }
  }

  /** The whole clip, mono i16 at [`SAMPLE_RATE`]. */
  take(): Int16Array {
    return Int16Array.from(this.held);
  }
}

/** One open microphone and the clip it is filling. */
export class VoiceCapture {
  private constructor(
    private readonly source: MicSource,
    readonly clip: HeldClip,
  ) {
    source.onFrame = (bytes) => clip.push(bytes);
  }

  /**
   * Open the device through whichever half owns it: the shell's native
   * capture on the desktop, the webview's own in a browser.
   */
  static async open(device: string | null): Promise<VoiceCapture> {
    const source = await openCapture(device);
    return new VoiceCapture(source, new HeldClip());
  }

  /**
   * End the hold and hand the clip over, letting the source's last frames
   * land first. The device stays held on the desktop, as it does for a take;
   * a browser closes its own and opens the next hold afresh.
   */
  async release(): Promise<HeldClip> {
    const source = this.source;
    source.flushWorklet();
    if (source.drain !== undefined) await source.drain();
    else await new Promise((resolve) => setTimeout(resolve, FLUSH_SETTLE_MS));
    const tail = source.flush();
    if (tail !== null) this.clip.push(tail);
    source.onFrame = null;
    source.stop();
    return this.clip;
  }

  /** Throw the hold away: the device is released and nothing is sent. */
  cancel(): void {
    this.source.onFrame = null;
    this.source.stop();
  }
}

/** Whether this platform captures through the shell rather than the page. */
export function shellCapture(): boolean {
  return isTauri();
}

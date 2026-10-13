/**
 * The desktop's microphone: the shell's own capture, framed in Rust.
 *
 * The page does not open a device here. The shell does, natively through
 * `forge-capture`, and hands the frames
 * back as `client://mic` events - the bytes of the very frames the browser's
 * capture sends, so the server and the take feed see no difference at all.
 * This side owns the take around them exactly as it does for a browser's
 * microphone: one take path, two sources.
 *
 * **Why it is not `getUserMedia`.** The webview's capture pays the browser's
 * own pipeline - a permission round trip, an audio graph, a resampling
 * context - before the first sample exists, and on a microphone that has gone
 * cold the device itself adds its wake on top. A native open starts at the
 * device, which is what the terminal did and what makes a press capture from
 * the press.
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import type { MicSource } from './take';

/** One live capture in the shell, as this side sees it. */
export class ShellMic implements MicSource {
  onFrame: ((bytes: Uint8Array) => void) | null = null;

  private constructor(private readonly hearing: UnlistenFn) {}

  /**
   * Open the picked input - or the system default when nothing is picked -
   * or report why it would not open.
   *
   * The listener goes up before the shell is asked to start, because the
   * first frames arrive as soon as the device does.
   */
  static async open(device: string | null): Promise<ShellMic> {
    const held: { mic: ShellMic | null } = { mic: null };
    const hearing = await listen<number[]>('client://mic', (event) => {
      held.mic?.onFrame?.(new Uint8Array(event.payload));
    });
    try {
      await invoke('client_mic_start', { device });
    } catch (why) {
      hearing();
      throw why;
    }
    const mic = new ShellMic(hearing);
    held.mic = mic;
    return mic;
  }

  /**
   * The take's last partial frame, if this side held one.
   *
   * It never does: the shell frames the audio, and it hands its own partial
   * over on the drain - where this side could not reach it at all.
   */
  flush(): Uint8Array | null {
    return null;
  }

  /** Nothing to ask for: the shell's own drain is what hands the rest over. */
  flushWorklet(): void {
    // The framing lives in the shell's half.
  }

  /**
   * Ask the shell to drain its pipeline, hand over the frames still in it,
   * and release the device.
   *
   * **This waits on the shell's answer, not on the frames.** Frames cross as
   * events and the answer as this invoke's reply, and the two channels are
   * not ordered against each other: the last partial frame (up to 20 ms) can
   * land after this resolves, and the stop the take sends next can overtake
   * it. The drained audio - the part that was being cut - is out by then, so
   * what that costs is one partial frame, bounded.
   */
  async drain(): Promise<void> {
    await invoke('client_mic_drain');
  }

  /** Let the device go at once, for a take whose audio is being thrown away. */
  stop(): void {
    this.hearing();
    void invoke('client_mic_stop').catch(() => {
      // The shell is gone, and the device went with it.
    });
  }
}

/** The shell's capture, as a take opens it. */
export function openShellMic(device: string | null): Promise<MicSource> {
  return ShellMic.open(device);
}

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

import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import { mintPromptId } from '../wire/ids';
import type { MicSource } from './take';

/**
 * This page load's own id, stated on the hold and the press.
 *
 * It is what lets the shell end a take a reload stranded: a reload discards
 * the page that held the take's stop, so a recording another page finds is
 * one no live page owns, and the shell ends it rather than keeping it. One id
 * per call would make each call a new page, and a page's own pick change
 * mid-take would end its own recording.
 *
 * Minted through the guarded helper, because `crypto.randomUUID` is
 * secure-context-only and the web half is served over plain http: a raw call
 * at module scope throws before the app draws anything.
 */
const page = mintPromptId();

/** One live capture in the shell, as this side sees it. */
export class ShellMic implements MicSource {
  onFrame: ((bytes: Uint8Array) => void) | null = null;

  private constructor(
    private readonly hearing: UnlistenFn,
    /**
     * The input the shell opened, which is the one moment a plain take names
     * the system default: the picker's own list carries no default mark, so
     * the take learns it from here and remembers it, as it does on the web.
     */
    readonly resolved: { id: string; label: string },
    /**
     * This take's own token on the wire, minted at the open. The shell
     * matches the take's stop and drain against it, which the page id could
     * not do: a second take of the same page is a different token.
     */
    private readonly take: string,
  ) {}

  /**
   * Open the picked input - or the system default when nothing is picked -
   * or report why it would not open.
   *
   * The listener goes up before the shell is asked to start, because the
   * first frames arrive as soon as the device does. The shell answers the
   * start with the input it opened.
   */
  static async open(device: string | null): Promise<ShellMic> {
    const take = mintPromptId();
    const held: { mic: ShellMic | null } = { mic: null };
    const hearing = await listen<number[]>('client://mic', (event) => {
      held.mic?.onFrame?.(new Uint8Array(event.payload));
    });
    let resolved: { id: string; label: string };
    try {
      resolved = await invoke<{ id: string; label: string }>('client_mic_start', {
        device,
        page,
        take,
      });
    } catch (why) {
      hearing();
      throw why;
    }
    const mic = new ShellMic(hearing, resolved, take);
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
   * and end the take. The device stays held, so the next press is warm.
   *
   * **This waits on the shell's answer, not on the frames.** Frames cross as
   * events and the answer as this invoke's reply, and the two channels are
   * not ordered against each other: the last partial frame (up to 20 ms) can
   * land after this resolves, and the stop the take sends next can overtake
   * it. The drained audio - the part that was being cut - is out by then, so
   * what that costs is one partial frame, bounded.
   */
  async drain(): Promise<void> {
    await invoke('client_mic_drain', { take: this.take });
  }

  /**
   * End the take at once, for audio that is being thrown away. The device
   * stays held, so the next press is warm.
   *
   * It names this take's own token, so one landing after the take ended -
   * the page re-pressed - ends nothing.
   */
  stop(): void {
    this.hearing();
    void invoke('client_mic_stop', { take: this.take }).catch(() => {
      // The shell is gone, and the held device went with it.
    });
  }
}

/** The shell's capture, as a take opens it. */
export function openShellMic(device: string | null): Promise<MicSource> {
  return ShellMic.open(device);
}

/**
 * Keep the picked input open and warm in the shell, so the next press
 * records from the press.
 *
 * Called where the input in force becomes known - the surface that takes
 * typing, as it mounts, and the panel on every pick - and it is the shell
 * that holds the device from there: a take consumes the held stream without
 * closing it, and the hold outlives every take until the client exits.
 *
 * The cost is the microphone indicator staying lit while the client runs.
 * A hold that will not open is not drawn: the shell logs it, and the next
 * press opens the device itself, exactly as it did before there was a hold.
 */
export async function holdShellMic(device: string | null): Promise<void> {
  if (!isTauri()) return;
  try {
    await invoke('client_mic_hold', { device, page });
  } catch {
    // The shell's own log carries why; the press falls back to a cold open.
  }
}

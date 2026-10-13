/**
 * The microphone: the inputs this device offers, and one open stream.
 *
 * Browser plumbing, kept out of `capture.ts` so the encoding and the ring
 * stay testable with no audio stack in the test run. The desktop's own
 * capture is `mic-shell.ts`; what lives here is the browser's, plus the one
 * list a panel asks for, whichever half can answer it.
 */

import { invoke, isTauri } from '@tauri-apps/api/core';

import { FrameChunker, SAMPLE_RATE } from './capture.svelte';

/** An input this client can record from. */
export interface Input {
  /** Stable identity, which is what a pick persists. */
  id: string;
  /** Human label for a picker. Empty until the browser's origin has been
   * granted the microphone once - the browser's own rule, not a failure. */
  label: string;
}

/**
 * Every input this client can record from.
 *
 * On the desktop that is the shell's own list, because the shell is what
 * opens the device there: its ids are the ones a take resolves, and the
 * browser's own ids mean nothing to it. In a browser it is
 * `enumerateDevices`, which needs no permission - the LABELS do, so a fresh
 * origin lists unnamed rows until a take has been granted once.
 */
export async function inputs(): Promise<Input[]> {
  if (isTauri()) {
    const listed = await invoke<{ id: string; label: string }[]>('client_mic_devices');
    return listed.map((held) => ({ id: held.id, label: held.label }));
  }
  const devices = await navigator.mediaDevices.enumerateDevices();
  return devices
    .filter((device) => device.kind === 'audioinput')
    .map((device) => ({ id: device.deviceId, label: device.label }));
}

/** Why a walk of the inputs came back empty-handed. */
export function inputLine(why: unknown): string {
  const name = why instanceof DOMException ? why.name : '';
  if (name === 'NotAllowedError' || name === 'SecurityError') {
    return 'the browser would not list inputs \u{b7} allow the microphone and try again';
  }
  return 'the inputs could not be listed \u{b7} try again';
}

/** One open microphone, and the worklet feeding it. */
export class Microphone {
  /** Every 20 ms chunk, encoded. Set before `start` has anything to send. */
  onFrame: ((bytes: Uint8Array) => void) | null = null;

  private constructor(
    private readonly stream: MediaStream,
    private readonly context: AudioContext,
    private readonly node: AudioWorkletNode,
    private readonly chunker: FrameChunker,
    /**
     * The input the stream actually opened, which is the only moment the
     * browser names the system default: a stream asked for WITH a pick is
     * that device, and one asked for plainly is whatever the OS resolves to.
     */
    readonly resolved: { id: string; label: string },
  ) {}

  /**
   * Open the picked input, or the system default when `deviceId` is null.
   *
   * A device that is named and gone is an error rather than a fallback:
   * someone who chose an interface needs to know it is not there, not to
   * be quietly recorded on something else.
   */
  static async open(deviceId: string | null): Promise<Microphone> {
    const audio: MediaTrackConstraints = { channelCount: 1 };
    if (deviceId !== null) audio.deviceId = { exact: deviceId };
    const stream = await navigator.mediaDevices.getUserMedia({ audio });
    const track = stream.getAudioTracks()[0];
    const settings = track?.getSettings() ?? {};
    const resolved = {
      id: typeof settings.deviceId === 'string' ? settings.deviceId : '',
      label: track?.label ?? '',
    };

    // The context at the model's own rate, so the browser's resampler is
    // the only one in the path and everything downstream is mono 16 kHz.
    const context = new AudioContext({ sampleRate: SAMPLE_RATE });
    await context.audioWorklet.addModule(new URL('./mic-worklet.js', import.meta.url));
    const node = new AudioWorkletNode(context, 'forge-mic', { numberOfOutputs: 0 });
    const chunker = new FrameChunker();
    const mic = new Microphone(stream, context, node, chunker, resolved);
    node.port.onmessage = (event: MessageEvent<Float32Array>) => {
      if (mic.onFrame === null) return;
      for (const frame of chunker.push(event.data)) mic.onFrame(frame);
    };
    context.createMediaStreamSource(stream).connect(node);
    return mic;
  }

  /** The take's last partial frame, if the microphone still holds one. */
  flush(): Uint8Array | null {
    return this.chunker.flush();
  }

  /**
   * Ask the worklet to post what it is holding.
   *
   * The samples it has gathered since the last chunk are newer than anything
   * this side has, and stopping the graph drops them - so a take asks for
   * them before it lets go, and they arrive as one short chunk.
   */
  flushWorklet(): void {
    this.node.port.postMessage('flush');
  }

  /** Release the device and close the context. */
  stop(): void {
    this.node.port.onmessage = null;
    this.node.disconnect();
    for (const track of this.stream.getTracks()) track.stop();
    void this.context.close();
  }
}

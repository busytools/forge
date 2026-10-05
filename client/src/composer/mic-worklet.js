/**
 * The microphone's audio, gathered into the 20 ms chunks the wire carries.
 *
 * A worklet rather than a main-thread handler: the callback runs on the
 * audio thread, so a busy page cannot drop the reader's words. Downmixing
 * here is defensive - the stream is asked for one channel - because
 * discarding a capsule would halve the signal on hardware where the
 * speaker sits nearer the other.
 *
 * Plain JavaScript on purpose: `audioWorklet.addModule` fetches this file
 * as it stands, with no bundler in front of it.
 */
const CHUNK = 320; // 20 ms at the 16 kHz context `mic.ts` opens.

class ForgeMic extends AudioWorkletProcessor {
  constructor() {
    super();
    this.held = new Float32Array(CHUNK);
    this.at = 0;
  }

  /**
   * @param {Float32Array[][]} inputs
   * @returns {boolean}
   */
  process(inputs) {
    const input = inputs[0];
    if (!input || input.length === 0 || !input[0]) return true;
    const channels = input.length;
    const frames = input[0].length;
    for (let at = 0; at < frames; at += 1) {
      let sum = 0;
      for (let channel = 0; channel < channels; channel += 1) {
        sum += input[channel][at];
      }
      this.held[this.at] = sum / channels;
      this.at += 1;
      if (this.at === CHUNK) {
        // A fresh buffer per chunk: the one posted is the caller's, and
        // this thread starts filling the next one immediately.
        this.port.postMessage(this.held);
        this.held = new Float32Array(CHUNK);
        this.at = 0;
      }
    }
    return true;
  }
}

registerProcessor('forge-mic', ForgeMic);

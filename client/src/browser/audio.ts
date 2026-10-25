/**
 * The page's sound, played by the phone's page.
 *
 * **The desktop hears it in the shell** (a native player owns that device);
 * on Android the shell hands each packet to the page, which plays it with
 * Web Audio because a webpage is where that device is reachable. Packets are
 * scheduled end to end so a stream plays without a gap per packet.
 */

let context: AudioContext | null = null;
let nextAt = 0;

/** The bytes a base64 string carries. */
function bytesOf(base64: string): Uint8Array {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let at = 0; at < binary.length; at += 1) bytes[at] = binary.charCodeAt(at);
  return bytes;
}

/**
 * The packet's samples as a playable buffer: interleaved 16-bit little-endian
 * becomes the format's planar float, scaled the way the wire scaled it.
 */
export function soundBuffer(
  make: (channels: number, frames: number, rate: number) => AudioBuffer,
  base64: string,
  rate: number,
  channels: number,
): AudioBuffer | null {
  if (channels <= 0 || rate <= 0) return null;
  const bytes = bytesOf(base64);
  const samples = new Int16Array(bytes.buffer, bytes.byteOffset, Math.floor(bytes.length / 2));
  const frames = Math.floor(samples.length / channels);
  if (frames <= 0) return null;
  const buffer = make(channels, frames, rate);
  for (let channel = 0; channel < channels; channel += 1) {
    const plane = buffer.getChannelData(channel);
    for (let frame = 0; frame < frames; frame += 1) {
      plane[frame] = (samples[frame * channels + channel] ?? 0) / 32767;
    }
  }
  return buffer;
}

/** Play one packet, scheduled after whatever is already playing. */
export function playPacket(base64: string, rate: number, channels: number): void {
  if (typeof AudioContext === 'undefined') return;
  try {
    context ??= new AudioContext();
    void context.resume();
    const buffer = soundBuffer(
      (count, frames, sampleRate) =>
        new AudioBuffer({ numberOfChannels: count, length: frames, sampleRate }),
      base64,
      rate,
      channels,
    );
    if (buffer === null) return;
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    const at = Math.max(context.currentTime, nextAt);
    source.start(at);
    nextAt = at + buffer.duration;
  } catch {
    // A refused context is a silent page, not a broken one.
  }
}

/**
 * The `Voice` a seat's control holds, kept for the page's life rather than the
 * control's.
 *
 * **An ask swaps the composer's body and unmounts the switch with it**, which
 * is not the reader leaving the seat: a control-owned `Voice` would send
 * `voice_mode off` on every permission, question or hand-off and post a
 * "stopped listening" row the reader never asked for. The composer drops the
 * seat's entry when the reader moves, and that is the only place the switch is
 * turned off for a seat.
 */

import { Voice, type OpenCapture, type VoicePort } from './voice.svelte';

const voices = new Map<string, Voice>();

/** The seat's `Voice`, created on the first mount and reused by a remount. */
export function voiceFor(key: string, port: VoicePort, open: OpenCapture | undefined): Voice {
  const existing = voices.get(key);
  if (existing !== undefined) return existing;
  const voice = new Voice(port, open);
  voices.set(key, voice);
  return voice;
}

/** The reader left the seat: turn its voice off and forget the instance. */
export function dropVoice(key: string): void {
  const voice = voices.get(key);
  if (voice === undefined) return;
  voices.delete(key);
  voice.detach();
}

/** Test-only: drop every cached voice without sending anything. */
export function resetVoices(): void {
  voices.clear();
}

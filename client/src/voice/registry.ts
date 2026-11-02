/**
 * The `Voice` a seat's control holds, kept for the page's life rather than the
 * control's.
 *
 * **An ask swaps the composer's body and unmounts the switch with it**, which
 * is not the reader leaving the seat: a control-owned `Voice` would send
 * `voice_mode off` on every permission, question or hand-off and post a
 * "stopped listening" row the reader never asked for. The composer pauses the
 * seat's entry when the reader moves, and that is the only place the switch is
 * turned off for a seat.
 */

import { Voice, type VoicePort } from './voice.svelte';

const voices = new Map<string, Voice>();

/** The seat's `Voice`, created on the first mount and reused by a remount. */
export function voiceFor(key: string, port: VoicePort): Voice {
  const existing = voices.get(key);
  if (existing !== undefined) return existing;
  const voice = new Voice(port);
  voices.set(key, voice);
  return voice;
}

/**
 * The reader left this seat: its voice stops and tells the server it is no
 * longer heard, but the instance stays, so a return to a seat the reader had
 * on reconnects. Only the seat being left is touched - the one being entered
 * may already be cached, and pausing it would race its own resume.
 */
export function pauseVoice(key: string): void {
  voices.get(key)?.pause();
}

/**
 * The connection was replaced: every cached voice closes over the old one.
 *
 * **A `Voice` holds its port**, so the old server's store and dispatch. Kept
 * across a connect-elsewhere it draws `listening` over a socket that is gone.
 * Nothing is sent: that server dropped this client from its listener sets when
 * the socket closed.
 */
export function forgetVoices(): void {
  for (const voice of voices.values()) voice.forget();
  voices.clear();
}

/** Test-only: drop every cached voice without sending anything. */
export function resetVoices(): void {
  voices.clear();
}

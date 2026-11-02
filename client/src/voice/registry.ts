/**
 * The `Voice` a seat's control holds, kept for the page's life rather than the
 * control's: an ask unmounts the switch without the reader leaving the seat.
 */

import type { SessionSlot } from '../wire/types';
import { Voice, type VoicePort } from './voice.svelte';

const voices = new Map<string, Voice>();
const seats = new Map<string, SessionSlot>();

/** The seat's `Voice`, created on the first mount and reused by a remount. */
export function voiceFor(key: string, slot: SessionSlot, port: VoicePort): Voice {
  const existing = voices.get(key);
  if (existing !== undefined) return existing;
  const voice = new Voice(port);
  voices.set(key, voice);
  seats.set(key, slot);
  return voice;
}

/**
 * The seat this client has voice on for, when it has one: where a preview's
 * audio must land, because the settings page has no seat of its own.
 *
 * The last one turned on wins, which is the one the reader was listening to
 * when they left the seat.
 */
export function listeningVoice(): { slot: SessionSlot; voice: Voice } | null {
  let found: { slot: SessionSlot; voice: Voice } | null = null;
  for (const [key, voice] of voices) {
    const slot = seats.get(key);
    if (voice.on && slot !== undefined) found = { slot, voice };
  }
  return found;
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
  seats.clear();
}

/** Test-only: drop every cached voice without sending anything. */
export function resetVoices(): void {
  voices.clear();
  seats.clear();
}

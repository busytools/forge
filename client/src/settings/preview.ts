/** The preview row's own state: which voice the page asked for, and where
 * that press is. Pure, so the transitions are testable without an audio
 * device behind them. */

export type PreviewPhase = 'working' | 'playing' | 'failed';

export interface PreviewState {
  voice: string;
  phase: PreviewPhase;
}

/** The next state of a press, or `null` once its clip has ended. `failure` is
 * the core's or the socket's words; `ignored` is the failure already on
 * screen when the press landed, which must not flip the fresh press. */
export function previewNext(
  held: PreviewState,
  playing: boolean,
  failure: string | null,
  ignored: string | null,
): PreviewState | null {
  if (failure !== null) {
    return failure !== ignored && held.phase !== 'failed'
      ? { voice: held.voice, phase: 'failed' }
      : held;
  }
  if (playing) {
    return held.phase === 'playing' ? held : { voice: held.voice, phase: 'playing' };
  }
  return held.phase === 'playing' ? null : held;
}

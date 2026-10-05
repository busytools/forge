<script lang="ts">
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { meterWindow } from './meter';
  import type { Take } from './wire';

  let {
    take,
    slot,
    connection,
    wire = null,
  }: {
    take: Take;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch'>;
    /**
     * The take's wire side, when this page owns the capture: frames produced
     * and bytes the socket has taken. `null` for a take this page did not
     * start, since the count is the producer's own fact.
     */
    wire?: { frames: number; bytes: number } | null;
  } = $props();

  const transcribing = $derived(take.phase === 'transcribing');
  const cells = $derived(meterWindow(take.levels));

  /** How long the take has run, as a reader reads it. */
  const clock = $derived(
    `${Math.floor(take.elapsedMs / 60_000)}:${String(Math.floor((take.elapsedMs % 60_000) / 1000)).padStart(2, '0')}`,
  );

  /** The live level, which holds its last reading once the audio has stopped. */
  const level = $derived(`${Math.round(take.peakDb)} dB`);

  /**
   * The wire line: what the capture has produced and what has left. The
   * numbers are the ring's own signals, so this recomputes as the take runs.
   */
  const wireLine = $derived.by(() =>
    wire === null ? null : `${wire.frames} fr \u{b7} ${(wire.bytes / 1024).toFixed(1)} KB`,
  );

  /**
   * What the row says it is doing, in the terminal's own words.
   *
   * A take settles segments while the microphone is still open, so a live one
   * counts the ones ready behind the speaker - the terminal has said
   * `listening · 2 ready` there for as long as it has had this row. The total
   * is only known once the take closes, and naming it before then would be a
   * number the take cannot have.
   */
  const label = $derived.by(() => {
    const { done, total } = take.progress;
    if (!transcribing) return done > 0 ? `listening \u{b7} ${done} ready` : 'listening';
    return total === null ? 'transcribing' : `transcribing ${done}/${total}`;
  });

  /** Abandon the take. Release-to-submit is the box's own control, not this one. */
  function cancel(): void {
    void connection.dispatch({ dictate_stop: { key: slot, submit: false } });
  }
</script>

<div class="dict">
  <span class="dot" class:tr={transcribing}></span>
  <span class="t" class:tr={transcribing}>{clock}</span>
  <span class="db" class:tr={transcribing}>{level}</span>
  <span class="wave" class:tr={transcribing}>
    <span class="wtr">
      {#each cells as cell, at (at)}
        <i class={cell.tone} style={`height:${cell.height}%`}></i>
      {/each}
    </span>
  </span>
  <span class="lbl">{label}</span>
  {#if wireLine !== null}
    <span class="wire">{wireLine}</span>
  {/if}
  <button class="esc" type="button" title="abandon the take" onclick={cancel}>
    <kbd aria-hidden="true">Esc</kbd> cancel
  </button>
</div>

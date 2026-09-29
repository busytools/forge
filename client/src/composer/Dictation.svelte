<script lang="ts">
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { meterCells } from './meter';
  import type { Take } from './wire';

  let {
    take,
    slot,
    connection,
  }: {
    take: Take;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch'>;
  } = $props();

  const transcribing = $derived(take.phase === 'transcribing');
  const cells = $derived(meterCells(take.levels));

  /** How long the take has run, as a reader reads it. */
  const clock = $derived(
    `${Math.floor(take.elapsedMs / 60_000)}:${String(Math.floor((take.elapsedMs % 60_000) / 1000)).padStart(2, '0')}`,
  );

  /** The live level, which holds its last reading once the audio has stopped. */
  const level = $derived(`${Math.round(take.peakDb)} dB`);

  /**
   * What the row says it is doing.
   *
   * A take settles segments while the microphone is still open, so a live one
   * reports how many are in; the total is only known once the take closes, and
   * naming it before then would be a number the take cannot have.
   */
  const label = $derived(
    transcribing
      ? take.progress.total === null
        ? 'transcribing'
        : `transcribing ${take.progress.done}/${take.progress.total}`
      : 'listening',
  );

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
  <button class="esc" type="button" title="abandon the take" onclick={cancel}>
    <kbd aria-hidden="true">Esc</kbd> cancel
  </button>
</div>

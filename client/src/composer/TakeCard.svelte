<script lang="ts">
  import { meterWindow } from './meter';
  import type { Take } from './wire';

  let {
    take,
    wire,
    oncancel,
  }: {
    take: Take;
    /**
     * The take's wire side, when this page owns the capture: frames produced,
     * bytes the socket has taken, the pace, and the frames the ring is holding
     * while the socket is down. `null` for a take this page did not start,
     * since the counts are the producer's own fact.
     */
    wire: {
      frames: number;
      bytes: number;
      rate: number | null;
      held: number;
    } | null;
    oncancel: () => void;
  } = $props();

  /**
   * How many level readings the card's graph draws - the fixed pitch the
   * meter keeps, sized for the graph the card carries rather than the
   * console's own window.
   */
  const CARD_CELLS = 40;

  /** Above this many sections the ticks stop being ticks and the bar fills. */
  const SECTION_TICKS = 8;

  const transcribing = $derived(take.phase === 'transcribing');
  const done = $derived(take.progress.done);
  const total = $derived(take.progress.total);
  const cells = $derived(meterWindow(take.levels, CARD_CELLS));

  /** How long the take has run, as a reader reads it. */
  const clock = $derived(
    `${Math.floor(take.elapsedMs / 60_000)}:${String(Math.floor((take.elapsedMs % 60_000) / 1000)).padStart(2, '0')}`,
  );

  /** How far the sections have got, as the fill bar's own width. */
  const filled = $derived(total === null || total === 0 ? 0 : Math.round((done / total) * 100));

  /**
   * Every wire reading, read together.
   *
   * The frame count is the one counter the ring makes a signal; the bytes, the
   * pace and the hold are its plain readings beside it. They are read in one
   * derivation so a change in ANY of them lands here: the pace and the hold
   * are computed when the card repaints, so a socket going down or catching up
   * has to bring a repaint with it, and the counter that moves every frame is
   * that repaint's cause.
   *
   * The card is a reading, not a live region: these move about fifty times a
   * second, and an `aria-live` on any of them would read a take out loud frame
   * by frame. The state changes that matter - a take starting, landing, being
   * refused - are announced by the composer's own rows.
   */
  const live = $derived.by(() => {
    if (wire === null) return null;
    return {
      frames: wire.frames,
      bytes: `${(wire.bytes / 1024).toFixed(1)} KB`,
      pace: wire.rate === null ? null : `${String(Math.round(wire.rate / 1024))} KB/s`,
      held: wire.held,
    };
  });
</script>

<div class="tc" class:tr={transcribing}>
  {#if transcribing}
    <span class="spin" aria-hidden="true"></span>
  {:else}
    <span class="dot" aria-hidden="true"></span>
  {/if}
  <span class="clock">{clock}</span>
  {#if transcribing}
    {#if total === null}
      <!-- The take has closed but the count is not in yet: motion without a
           claim about how far along it is. -->
      <span class="bar sweep" aria-hidden="true"><i></i></span>
      <span class="count">transcribing</span>
    {:else if total <= SECTION_TICKS}
      <span class="ticks" aria-hidden="true">
        {#each Array.from({ length: total }, (_, at) => at) as at (at)}
          <i class:on={at < done}></i>
        {/each}
      </span>
      <span class="count">{done} of {total}</span>
    {:else}
      <span class="bar" aria-hidden="true"><i style={`width:${String(filled)}%`}></i></span>
      <span class="count">{done} of {total}</span>
    {/if}
  {:else}
    <span class="bars" aria-hidden="true">
      {#each cells as cell, at (at)}
        <i class={cell.tone} style={`height:${String(cell.height)}%`}></i>
      {/each}
    </span>
    {#if done > 0}<span class="ready">{done} ready</span>{/if}
  {/if}
  {#if live !== null}
    <span class="fr">{live.frames} fr</span>
    {#if !transcribing}
      {#if live.held > 0}
        <!-- The one reading that says the audio is not leaving. -->
        <span class="held">holding {live.held} fr</span>
      {:else}
        <span class="kb">{live.bytes}</span>
        {#if live.pace !== null}<span class="pace">{live.pace}</span>{/if}
      {/if}
    {/if}
  {/if}
  <button
    class="x"
    type="button"
    title="abandon the take"
    aria-label="abandon the take"
    onclick={oncancel}
  >
    &#10005;
  </button>
</div>

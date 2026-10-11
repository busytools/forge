<script lang="ts">
  import { fractionOf, meterWindow } from './meter';
  import type { Take } from './wire';

  let {
    take,
    wire,
    oncancel,
    spread = false,
  }: {
    take: Take;
    /**
     * The take's wire side, when this page owns the capture: frames produced,
     * bytes the socket has taken, the pace, and this side's own levels and
     * clock. `null` for a take this page did not start, since the counts are
     * the producer's own fact.
     */
    wire: {
      frames: number;
      bytes: number;
      rate: number | null;
      dbfs: number[];
      elapsedMs: number;
    } | null;
    oncancel: () => void;
    /**
     * Whether the card spans a line - the composer's take line, the dock's
     * words row - rather than sitting as the pill. **The graph follows the
     * shape**: the pill keeps its per-cell bars, and a spread card draws the
     * same readings as one smooth envelope (Ved, 2026-10-11: at the row's
     * width the cells' gaps read as chunks, and a path through the same
     * readings moves as a line).
     */
    spread?: boolean;
  } = $props();

  /**
   * How many level readings the card's graph draws - the fixed pitch the
   * meter keeps, sized for the graph the card carries rather than the
   * console's own window.
   */
  const CARD_CELLS = 40;

  /** Above this many sections the ticks stop being ticks and the bar fills. */
  const SECTION_TICKS = 8;

  /**
   * The frames produced, inside the eight characters the count slots hold.
   *
   * A take runs 50 frames a second against a 30-minute cap, so the count
   * reaches five digits at a few minutes and ~90k at the cap: past ten
   * thousand it is rounded to thousands, which is the reading a diagnostic
   * needs and all the card can hold.
   */
  function count(frames: number): string {
    return frames < 10_000 ? `${String(frames)} fr` : `${String(Math.round(frames / 1000))}k fr`;
  }

  /** The bytes taken, in the unit that keeps the reading under eight characters. */
  function size(bytes: number): string {
    return bytes < 1_000_000
      ? `${(bytes / 1024).toFixed(1)} KB`
      : `${(bytes / 1_048_576).toFixed(1)} MB`;
  }

  const transcribing = $derived(take.phase === 'transcribing');
  const done = $derived(take.progress.done);
  const total = $derived(take.progress.total);

  /**
   * Every reading this side produced, read together.
   *
   * The frame count and the bytes are the ring's own signals; the pace, the
   * levels and the clock are its plain readings, computed when the card
   * repaints. They are read in one derivation so a change in ANY of them lands
   * here: a flush moves the bytes without moving the frames, and a frame moves
   * the frames without necessarily moving the bytes, so the pace and the
   * levels have to ride whichever of the two moved.
   *
   * The counts are deliberately not a live region: they move about fifty times
   * a second, and an `aria-live` on any of them would read a take out loud
   * frame by frame. The phase has a status of its own, which changes once per
   * take state rather than once per frame.
   */
  const live = $derived.by(() => {
    if (wire === null) return null;
    return {
      frames: count(wire.frames),
      bytes: size(wire.bytes),
      pace: wire.rate === null ? null : `${String(Math.round(wire.rate / 1024))} KB/s`,
      dbfs: wire.dbfs,
      elapsedMs: wire.elapsedMs,
    };
  });

  /**
   * The levels the graph draws: this side's own reading of the frames it
   * produced when it owns the capture, and the record's when the take belongs
   * to a page that did not capture it - the one case where the wire's `peak_db`
   * is the only source there is.
   */
  const levels = $derived(
    live === null ? take.levels : live.dbfs.map((peakDb) => fractionOf(peakDb, take.floorDb)),
  );
  const cells = $derived(meterWindow(levels, CARD_CELLS));

  /**
   * The same readings as one smooth envelope, for a card that spans a line.
   *
   * A Catmull-Rom curve through the cells' tops, drawn into a 0..100 box the
   * SVG stretches to whatever width and height the row gives it - so the
   * readings scale with the row instead of sitting in a fixed track, and one
   * path moves where forty bars stepped. Control points can overshoot the box
   * on a spike, which the viewBox clips rather than the card drawing outside
   * itself.
   */
  const waveLine = $derived.by(() => {
    const n = cells.length;
    if (n === 0) return '';
    const x = (at: number): number => (at / (n - 1)) * 100;
    const y = (at: number): number => 100 - (cells[at]?.height ?? 0);
    let d = `M ${String(x(0))} ${String(y(0))}`;
    for (let at = 0; at < n - 1; at += 1) {
      const lo = Math.max(0, at - 1);
      const hi = Math.min(n - 1, at + 2);
      const c1x = x(at) + (x(at + 1) - x(lo)) / 6;
      const c1y = y(at) + (y(at + 1) - y(lo)) / 6;
      const c2x = x(at + 1) - (x(hi) - x(at)) / 6;
      const c2y = y(at + 1) - (y(hi) - y(at)) / 6;
      d += ` C ${String(c1x)} ${String(c1y)} ${String(c2x)} ${String(c2y)} ${String(x(at + 1))} ${String(y(at + 1))}`;
    }
    return d;
  });
  const waveArea = $derived(waveLine === '' ? '' : `${waveLine} L 100 100 L 0 100 Z`);

  /**
   * How long the take has run, as a reader reads it - this side's own clock
   * for a take it started, the record's for one it did not.
   */
  const elapsed = $derived(live === null ? take.elapsedMs : live.elapsedMs);
  const clock = $derived(
    `${Math.floor(elapsed / 60_000)}:${String(Math.floor((elapsed % 60_000) / 1000)).padStart(2, '0')}`,
  );

  /** How far the sections have got, as the fill bar's own width. */
  const filled = $derived(total === null || total === 0 ? 0 : Math.round((done / total) * 100));
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
    {#if spread}
      <svg
        class="wave"
        viewBox="0 0 100 100"
        preserveAspectRatio="none"
        aria-hidden="true"
        role="presentation"
      >
        <path class="area" d={waveArea} />
        <path class="edge" d={waveLine} />
      </svg>
    {:else}
      <span class="bars" aria-hidden="true">
        {#each cells as cell, at (at)}
          <i class={cell.tone} style={`height:${String(cell.height)}%`}></i>
        {/each}
      </span>
    {/if}
    {#if done > 0}<span class="ready">{done} ready</span>{/if}
  {/if}
  {#if live !== null}
    <span class="fr">{live.frames}</span>
    {#if !transcribing}
      <span class="kb">{live.bytes}</span>
      {#if live.pace !== null}<span class="pace">{live.pace}</span>{/if}
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
  <!-- One status per phase, not per frame: the counts above move fifty times a
       second and are silent on purpose, so this is the only thing a screen
       reader hears from the card. -->
  <span class="sr" role="status">{transcribing ? 'transcribing the take' : 'dictating'}</span>
</div>

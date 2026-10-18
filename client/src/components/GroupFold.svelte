<script lang="ts">
  import type { Snippet } from 'svelte';

  import Chevron from './Chevron.svelte';

  /**
   * A rail heading that folds what it holds: the asleep half of the fleet.
   *
   * **The open state is held here, and the rule is one-way.** A fold OPENS
   * when the seat the page is showing moves in behind it, and never closes on
   * its own - the reader's toggle is the only thing that closes it. Arriving on
   * a sleeping seat is the ordinary path (a deep link, a click from the
   * roster, a resume), and a fold that stayed shut over the one marked row
   * would show a count and no sign of where the reader is.
   *
   * The count is what the heading hides, so a folded group cannot read as an
   * empty one.
   */
  let {
    heading,
    count,
    mark = null,
    holds = false,
    startOpen = false,
    children,
  }: {
    heading: string;
    count: number;
    mark?: string | null;
    holds?: boolean;
    /**
     * Whether the fold starts open when nothing moves in behind it. The rail
     * keeps its asleep section folded; the home shows the same section
     * expanded, because there the fleet is the page rather than a column
     * beside it.
     */
    startOpen?: boolean;
    children: Snippet;
  } = $props();

  // Read once, through a call, as the inspector's own sections take theirs:
  // this is what the fold opens on first render, and the effect below is what
  // keeps it open when the seat arrives later.
  const startsOpen = () => holds || startOpen;
  let open = $state(startsOpen());
  $effect(() => {
    if (holds) open = true;
  });
</script>

<details class="gfold" bind:open>
  <summary>
    <span class="gh">{heading}</span>
    <span class="cn">{count}</span>
    <!-- The signal the rows inside carry, where the ages and close chips
         stand: a fold is display, so a mark inside it survives the fold. -->
    {#if mark !== null}<span class="dot {mark}"></span>{/if}
    <Chevron />
  </summary>
  {@render children()}
</details>

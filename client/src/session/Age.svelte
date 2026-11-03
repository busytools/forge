<script lang="ts">
  /**
   * The row's age cell, read against the page's clock.
   *
   * **The clock is a store and this is the leaf that reads it**, so a tick
   * re-renders the text it changes and not the row tree around it: the row's
   * builder hands over a raw stamp (`AgeReading`) and the cell turns it into
   * the short form here.
   */
  import type { Readable } from 'svelte/store';

  import { ageLabel, type AgeReading } from '../home/view';

  let { at, now }: { at: AgeReading; now: Readable<number> } = $props();
</script>

<span class="age">{ageLabel(at, $now)}</span>

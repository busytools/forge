<script lang="ts">
  import type { Readable } from 'svelte/store';

  import List from './List.svelte';

  /**
   * Two branches that each carry a list, so a test can swap them in one flush.
   *
   * **The order Svelte swaps in is the whole point.** The incoming list
   * registers only after the departing teardown has run, so the swap clears its
   * own handle and never reaches the guarded clear - a state the column cannot
   * reach anyway, because it draws one list in a single branch.
   */
  let { swapped }: { swapped: Readable<boolean> } = $props();
</script>

{#if $swapped}
  <List />
{:else}
  <List />
{/if}

<script lang="ts">
  import type { Readable } from 'svelte/store';

  import List from './List.svelte';

  /**
   * Two branches that each carry a list, so a test can swap them in one flush.
   *
   * **The order Svelte swaps in is the whole point.** It creates the incoming
   * branch before it destroys the outgoing one, so a list whose teardown clears
   * the seam unguarded takes its replacement's handle down with it - a state
   * the column cannot reach today, because it draws one list in a single branch.
   */
  let { swapped }: { swapped: Readable<boolean> } = $props();
</script>

{#if $swapped}
  <List />
{:else}
  <List />
{/if}

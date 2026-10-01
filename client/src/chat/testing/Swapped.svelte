<script lang="ts">
  import type { Readable } from 'svelte/store';

  import List from './List.svelte';

  /**
   * Two branches that each carry a list, so a test can swap them in one flush.
   *
   * **The order Svelte swaps in is the whole point.** It creates the incoming
   * branch before it destroys the outgoing one, so a list whose teardown clears
   * the seam unguarded takes its replacement's handle down with it - a state no
   * single-branch tree can reach, because one list is only ever destroyed with
   * nothing to replace it.
   */
  let { swapped }: { swapped: Readable<boolean> } = $props();
</script>

{#if $swapped}
  <List />
{:else}
  <List />
{/if}

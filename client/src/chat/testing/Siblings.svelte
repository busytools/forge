<script lang="ts">
  import type { Readable } from 'svelte/store';

  import List from './List.svelte';

  /**
   * Two lists under one parent, so a test can depart one while the other stays.
   *
   * **The departing list here is not the seam's holder, which is the one shape
   * the guarded clear covers** - the ordering and what the guard does not cover
   * are measured beside it in `List.svelte`.
   */
  let { both }: { both: Readable<boolean> } = $props();
</script>

{#if $both}
  <List />
{/if}
<List />

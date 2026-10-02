<script lang="ts">
  import type { Readable } from 'svelte/store';

  import List from './List.svelte';

  /**
   * Two lists under one parent, so a test can depart one while the other stays.
   *
   * **The survivor is the shape the guarded clear exists for.** The departing
   * list's teardown lands after the survivor registered, and nothing registers
   * after it - so a clear that did not check whose handle it held would take
   * the list still on screen down with it. A swap never reaches this, because
   * there the departing clear lands before the replacement registers.
   */
  let { both }: { both: Readable<boolean> } = $props();
</script>

{#if $both}
  <List />
{/if}
<List />

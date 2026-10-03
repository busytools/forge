<script lang="ts">
  import type { Readable } from 'svelte/store';

  import Call from '../Call.svelte';
  import { opensByDefault, type ToolLeaf } from '../leaves';

  /**
   * One call whose result arrives after the row mounted.
   *
   * The live wire delivers a `tool_use` first and its result later, while a
   * page re-read folds both at once - so this drives the LIVE order, the one
   * a row that snapshots its opener at mount cannot reach.
   */
  let {
    before,
    after,
    arrived,
  }: { before: ToolLeaf; after: ToolLeaf; arrived: Readable<boolean> } = $props();

  const call = $derived($arrived ? after : before);
</script>

<Call {call} k="arriving" open={opensByDefault(call.name, call.body, call.decision)} />

<script lang="ts">
  import Strip from './Strip.svelte';
  import SubagentSegment from './SubagentSegment.svelte';
  import type { TurnInfo } from './units';

  /**
   * The strip pinned above the box while the newest turn is being written.
   *
   * **It is a row of its own, and neither the composer nor the dock owns it**:
   * the client submits to the CLI and the CLI queues, so nothing else on the
   * page has a "prompt waiting" state of its own to hang this off.
   *
   * **It draws what it is handed and decides nothing.** Which row that is -
   * the running one, or the finished one held for the beat before it detaches
   * into the turn - is the conversation's own state, and it sits there because
   * one fact has two readers: this row, and the turn's own that must stand
   * aside while the pin holds it.
   */
  let { info }: { info: TurnInfo | null } = $props();
</script>

{#if info !== null}
  <div class="strip">
    <div class="ti"><Strip {info} /><SubagentSegment /></div>
  </div>
{/if}

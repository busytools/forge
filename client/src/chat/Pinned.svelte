<script lang="ts">
  import ConnectorsSegment from './ConnectorsSegment.svelte';
  import McpSegment from './McpSegment.svelte';
  import ProcessesSegment from './ProcessesSegment.svelte';
  import SchedulesSegment from './SchedulesSegment.svelte';
  import Strip from './Strip.svelte';
  import BrowserSegment from './BrowserSegment.svelte';
  import SubagentSegment from './SubagentSegment.svelte';
  import type { Connection } from '../socket';
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
  let { info, connection }: { info: TurnInfo | null; connection: Connection } = $props();
</script>

{#if info !== null}
  <div class="strip">
    <!-- The segments share one right-hand fill: rows each carrying their
         own auto margin would split the free space between them. -->
    <div class="ti">
      <Strip {info} />
      <span class="sg-fill"
        ><BrowserSegment {connection} /><SubagentSegment /><ProcessesSegment /><ConnectorsSegment
        /><SchedulesSegment /><McpSegment /></span
      >
    </div>
  </div>
{/if}

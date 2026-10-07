<script lang="ts">
  import ConnectorsSegment from './ConnectorsSegment.svelte';
  import GitSegment from './GitSegment.svelte';
  import McpSegment from './McpSegment.svelte';
  import MonitorsSegment from './MonitorsSegment.svelte';
  import ProcessesSegment from './ProcessesSegment.svelte';
  import SchedulesSegment from './SchedulesSegment.svelte';
  import Strip from './Strip.svelte';
  import BrowserSegment from './BrowserSegment.svelte';
  import SubagentSegment from './SubagentSegment.svelte';
  import TasksSegment from './TasksSegment.svelte';
  import { connectors } from './connectors.svelte';
  import { git } from './git.svelte';
  import { mcp } from './mcp.svelte';
  import { monitors } from './monitors.svelte';
  import { processes } from './processes.svelte';
  import { schedules } from './schedules.svelte';
  import { subagents } from './subagents.svelte';
  import { tasks } from './tasks.svelte';
  import type { Connection } from '../socket';
  import type { TurnInfo } from './units';

  /**
   * The strip pinned above the box: the turn being written, and the rows that
   * say what this seat holds.
   *
   * **The strip outlives the turn.** It used to draw only while a turn was
   * being written, which left an idle seat with no way at all into its tree,
   * its tasks or its watchers - and the rows are the home for what the
   * inspector used to hold. So the bar stands whenever it has anything to
   * say, the running turn being one of the things it may say.
   *
   * **It draws what it is handed and decides nothing about the turn.** Which
   * row that is - the running one, or the finished one held for the beat
   * before it detaches into the turn - is the conversation's own state, and
   * it sits there because one fact has two readers: this row, and the turn's
   * own that must stand aside while the pin holds it.
   */
  let { info, connection }: { info: TurnInfo | null; connection: Connection } = $props();

  const rows = $derived(
    subagents.all().length > 0 ||
      processes.anything() ||
      connectors.anything() ||
      schedules.anything() ||
      mcp.anything() ||
      git.anything() ||
      tasks.anything() ||
      monitors.anything(),
  );
</script>

{#if info !== null || rows}
  <div class="strip">
    <!-- The segments share one right-hand fill: rows each carrying their
         own auto margin would split the free space between them. -->
    <div class="ti">
      {#if info !== null}<Strip {info} />{/if}
      <span class="sg-fill"
        ><BrowserSegment {connection} /><SubagentSegment /><ProcessesSegment /><ConnectorsSegment
        /><SchedulesSegment /><McpSegment /><GitSegment /><TasksSegment /><MonitorsSegment /></span
      >
    </div>
  </div>
{/if}

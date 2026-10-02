<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import type { HookRun } from './units';

  /**
   * One hook's own run: the start that named it, what it printed as it went,
   * and the response that settled it.
   *
   * **Collapsed, carrying the hook and the state it reached**, for the reason a
   * thinking row is. A hook's output is long and secondary - a session-start
   * hook prints the boilerplate it injects into the session - so the whole of
   * it sits behind the row's own open rather than being drawn under every turn.
   * It is never clipped: a summary standing in for the output would be the drop
   * rule 25 forbids, which is why the row collapses it instead.
   */
  let { run }: { run: HookRun } = $props();
</script>

<details class="hookrun">
  <summary>
    {#if run.failed}<Icon name="x" class="st err" />{/if}
    <span class="hk">hook</span>
    <span class="nm">{run.name}</span>
    {#if run.event !== null}<span class="ev">({run.event})</span>{/if}
    <span class="ev">{run.state}</span>
    <Chevron />
  </summary>
  {#if run.body !== null}
    <div class="body">
      <div class="term">{run.body}</div>
    </div>
  {/if}
</details>

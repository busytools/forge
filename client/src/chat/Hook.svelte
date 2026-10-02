<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import type { HookRun } from './units';

  /**
   * One hook's own run: the start that named it, what it printed as it went,
   * and the response that settled it.
   *
   * **The terminal draws nothing for this**, which is why the shape is the
   * client's to choose rather than a parity port: its arm for the three frames
   * is a no-op. So the row takes the vocabulary the column already has for a
   * block nothing else shows - collapsed on what it is, with the whole of it
   * behind the row's own open - which is the shape `Thinking.svelte` uses for
   * the same reason.
   *
   * It is never clipped: a hook's output is long and secondary, so it is
   * collapsed rather than shortened, and a summary standing in for it would be
   * the drop rule 25 forbids.
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

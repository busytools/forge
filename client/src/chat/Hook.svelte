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
   * is a no-op. The row is a tool row like any other - the lane says the kind,
   * the mark says whether the run exited clean, and the name and the first line
   * of what it printed read as the call's own title does - because a hook IS
   * work the session ran, and anything else makes it a second system inside
   * the group.
   *
   * It is never clipped: a hook's output is long and secondary, so the row
   * collapses the whole of it rather than shortening it - the closed row's tail
   * is the output's own first line - and a summary standing in for the rest
   * would be the drop rule 25 forbids.
   */
  let { run }: { run: HookRun } = $props();

  /** The output's first non-blank line, which is the closed row's tail. */
  const headline = $derived(firstLine(run.body));

  function firstLine(body: string | null): string | null {
    if (body === null) return null;
    for (const line of body.split('\n')) {
      if (line.trim() !== '') return line.trim();
    }
    return null;
  }
</script>

<details class="leaf hookrow">
  <summary>
    {#if run.failed}
      <Icon name="x" class="st err" />
    {:else}
      <Icon name="check" class="st" />
    {/if}
    <span class="tn"
      >{run.name}{#if run.event !== null}
        ({run.event}){/if}</span
    >
    {#if headline !== null}
      <span class="ev">{headline}</span>
    {/if}
    <Chevron />
  </summary>
  {#if run.body !== null}
    <div class="body">
      <div class="term">{run.body}</div>
    </div>
  {/if}
</details>

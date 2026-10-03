<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { firstLine } from './text';
  import type { InboundLeaf } from './units';

  /**
   * One inbound delivery: a cron fire, a Slack message or a Gotify push.
   *
   * The row is a tool row like any other - the lane says the kind, and the
   * title and the body's first line read as the call's own title does -
   * because a delivery IS something the session received, and anything else
   * makes it a second system inside the group.
   *
   * It is never clipped: the whole of what arrived sits behind the row.
   */
  let { row }: { row: InboundLeaf } = $props();
</script>

<details class="leaf inboundrow">
  <summary>
    <span class="tn">{row.title}</span>
    {#if row.body !== ''}
      <span class="ev" class:warn={row.elevated}>{firstLine(row.body)}</span>
    {/if}
    <Chevron />
  </summary>
  {#if row.body !== ''}
    <div class="body">
      <div class="term">{row.body}</div>
    </div>
  {/if}
</details>

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

  /**
   * The tail line, or null where it would only repeat the title.
   *
   * **A cron fire's title IS its body's first line** (the fold reads it so),
   * so the row drew the same sentence twice - the first copy cut by its
   * one-line clamp, the second whole (Ved, 2026-10-03). A tail that adds
   * nothing to the title is not a tail, and everything the delivery carried
   * is still behind the row's own open.
   */
  const tail = $derived.by((): string | null => {
    if (row.body === '') return null;
    const line = firstLine(row.body);
    return line === row.title ? null : line;
  });
</script>

<details class="leaf inboundrow">
  <summary>
    <span class="tn">{row.title}</span>
    {#if tail !== null}
      <span class="ev" class:warn={row.elevated}>{tail}</span>
    {/if}
    <Chevron />
  </summary>
  {#if row.body !== ''}
    <div class="body">
      <div class="term">{row.body}</div>
    </div>
  {/if}
</details>

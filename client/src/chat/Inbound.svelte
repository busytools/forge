<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { firstLine, joinedLine } from './text';
  import type { InboundLeaf } from './units';

  /**
   * One inbound delivery: a cron fire, a Slack message or a Gotify push.
   *
   * The row is a tool row like any other - the lane says the kind, and the
   * title and the body's first line read as the call's own title does -
   * because a delivery IS something the session received, and anything else
   * makes it a second system inside the group.
   *
   * It is never clipped: the whole of what arrived sits behind the row, drawn
   * as prose - a delivery is a message meant to be read, so its marks render
   * rather than sitting on the page as themselves.
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
    if (line === row.title) return null;
    // A preview shows its inline marks, the way the hook row's own line does.
    return renderInlineProse(joinedLine(line));
  });

  /**
   * The title as the row draws it, with a cron fire's own line rendered.
   *
   * **A cron fire's title IS its body's first line**, so it carries the
   * delivery's markdown - drawn raw it showed the marks as themselves (#1708).
   * The other kinds' titles are names (a channel, an app) where a markdown
   * pass would rewrite what the name literally is, so it stays text.
   */
  const title = $derived(row.kind === 'cron' ? renderInlineProse(joinedLine(row.title)) : null);

  /** The kind's own glyph, which the row leads with. */
  const glyph = $derived(row.kind === 'cron' ? 'schedules' : row.kind);
</script>

<details class="leaf inboundrow">
  <summary>
    <Icon name={glyph} class="gl" />
    {#if title !== null}
      <!-- A cron fire's title, rendered from escaped input: the same renderer
           the body and the tail line use. -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <span class="tn">{@html title}</span>
    {:else}
      <span class="tn">{row.title}</span>
    {/if}
    {#if tail !== null}
      <!-- The row's rendered line, which the module produced from escaped
           input: the same renderer the body and a thought's line use. -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <span class="ev" class:warn={row.elevated}>{@html tail}</span>
    {/if}
    <Chevron />
  </summary>
  {#if row.body !== ''}
    <div class="body">
      <Prose text={row.body} />
    </div>
  {/if}
</details>

<script lang="ts">
  import type { Snippet } from 'svelte';

  /**
   * One instance's settings: a head band naming it, its key count and its
   * actions, with the setting lines beneath. A group with no instance name
   * draws no head band and hangs off the pane's own heading.
   */
  let {
    label = '',
    count = null,
    headingId = null,
    removed = false,
    fresh = false,
    actions,
    children,
  }: {
    label?: string;
    count?: number | null;
    /** The card heading's id; `null` draws no heading and names the pane's. */
    headingId?: string | null;
    /** A removal waiting on the save: the head stands, the lines do not. */
    removed?: boolean;
    /** A new entry's card: dashed, and drawn with its head even unnamed. */
    fresh?: boolean;
    actions?: Snippet;
    children?: Snippet;
  } = $props();
</script>

<div
  class="settings-card"
  class:removing={removed}
  class:new={fresh}
  role="group"
  aria-labelledby={headingId ?? 'settings-area-title'}
>
  {#if label !== ''}
    <header class="settings-card-head">
      {#if headingId === null}
        <span class="settings-card-name">{label}</span>
      {:else}
        <h3 id={headingId}>{label}</h3>
      {/if}
      {#if removed}
        <span class="settings-removed">removed on save</span>
      {:else if count !== null}
        <span class="settings-card-count">{count} {count === 1 ? 'key' : 'keys'}</span>
      {/if}
      <span class="settings-card-grow"></span>
      {@render actions?.()}
    </header>
  {/if}
  {#if !removed}{@render children?.()}{/if}
</div>

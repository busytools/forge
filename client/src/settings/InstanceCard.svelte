<script lang="ts">
  import type { Snippet } from 'svelte';

  import Chevron from '../components/Chevron.svelte';

  /**
   * One instance's settings: a head band naming it, its count and its
   * actions, with the setting lines beneath. A group with no instance name
   * draws no head band and hangs off the pane's own heading.
   *
   * **A foldable card's head band is its disclosure.** The same head band,
   * count and chevron, opened and closed by the summary: a section that
   * folds is still this page's card, not a second look for it.
   */
  let {
    label = '',
    count = null,
    countNoun = 'key',
    detail = null,
    headingId = null,
    removed = false,
    fresh = false,
    foldable = false,
    actions,
    children,
  }: {
    label?: string;
    count?: number | null;
    /** What the count counts: a key card says keys; `null` draws the bare
     * number, for a card whose own name already says what it counts. */
    countNoun?: string | null;
    /** A second fact beside the count, in the count's own words: the voices
     * card names the chosen voice here, so a shut card still says the pick. */
    detail?: string | null;
    /** The card heading's id; `null` draws no heading and names the pane's. */
    headingId?: string | null;
    /** A removal waiting on the save: the head stands, the lines do not. */
    removed?: boolean;
    /** A new entry's card: dashed, and drawn with its head even unnamed. */
    fresh?: boolean;
    /** The head band is a disclosure; the card starts shut. */
    foldable?: boolean;
    actions?: Snippet;
    children?: Snippet;
  } = $props();
</script>

{#snippet head()}
  {#if headingId === null}
    <span class="settings-card-name">{label}</span>
  {:else}
    <h3 id={headingId}>{label}</h3>
  {/if}
  {#if removed}
    <span class="settings-removed">removed on save</span>
  {:else if count !== null}
    <span class="settings-card-count"
      >{count}{countNoun === null ? '' : ` ${count === 1 ? countNoun : `${countNoun}s`}`}</span
    >
  {/if}
  {#if detail !== null}
    <span class="settings-card-count">{detail}</span>
  {/if}
  <span class="settings-card-grow"></span>
  {@render actions?.()}
{/snippet}

{#if foldable}
  <details
    class="settings-card"
    class:removing={removed}
    class:new={fresh}
    aria-labelledby={headingId ?? 'settings-area-title'}
  >
    <summary class="settings-card-head">
      {@render head()}
      <Chevron />
    </summary>
    {#if !removed}{@render children?.()}{/if}
  </details>
{:else}
  <div
    class="settings-card"
    class:removing={removed}
    class:new={fresh}
    role="group"
    aria-labelledby={headingId ?? 'settings-area-title'}
  >
    {#if label !== ''}
      <header class="settings-card-head">
        {@render head()}
      </header>
    {/if}
    {#if !removed}{@render children?.()}{/if}
  </div>
{/if}

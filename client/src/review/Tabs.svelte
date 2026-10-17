<script lang="ts">
  /**
   * The tab strip: one row of buttons, the half that is up underlined in the
   * accent - **one draw for the review page and the files page** (the lead's
   * gate, 2026-10-17: the two hand-written strips had drifted into copies).
   * A page's own controls ride beside the tabs through `extra`.
   */
  import type { Snippet } from 'svelte';

  let {
    tabs,
    active,
    label,
    onselect,
    extra,
  }: {
    tabs: { id: string; label: string; count?: number | null }[];
    active: string;
    /** The strip's own name for a screen reader: "review sections", "files
     *  sections". */
    label: string;
    onselect: (id: string) => void;
    /** The page's own controls on the same row, drawn after the tabs. */
    extra?: Snippet;
  } = $props();
</script>

<nav class="rv-tabs" aria-label={label}>
  {#each tabs as tab (tab.id)}
    <button type="button" class:on={active === tab.id} onclick={() => onselect(tab.id)}>
      {#if tab.count !== undefined && tab.count !== null}
        {tab.label} <span class="rv-tn">{tab.count}</span>
      {:else}
        {tab.label}
      {/if}
    </button>
  {/each}
  {#if extra}{@render extra()}{/if}
</nav>

<style>
  /* Narrow: the strip wraps rather than clipping a control at the edge -
     the review page's own rule, kept with the strip it belongs to. */
  @media (max-width: 980px) {
    nav {
      flex-wrap: wrap;
    }
  }
</style>

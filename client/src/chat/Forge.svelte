<script lang="ts">
  import type { ForgeCard } from './forge';
  import Icon from '../components/Icon.svelte';

  /**
   * One forge call's card: the facts its result carried, in the row grammar
   * every family shares - pairs of label and value, a quote of the words the
   * call carried, a list of what it found.
   *
   * The row itself - the glyph, the subject, the chips and the tail - is
   * drawn by `Call.svelte`, which owns the shell every call row sits in; this
   * component is the body and nothing else.
   */
  let { card, glyph }: { card: ForgeCard; glyph: string } = $props();
</script>

{#each card.pieces as piece, at (at)}
  {#if piece.kind === 'kv'}
    <!-- The facts as ONE inline run, the way a dispatch row states its own
         (`sg-meta`): a label with its value beside it, never a two-column
         grid that flings the value to the far edge. -->
    <div class="fam-meta">
      {#each piece.pairs as [label, value], pairAt (pairAt)}{#if pairAt > 0}<span class="sep"
            >·</span
          >{/if}<span class="lb">{label}</span>{value}{/each}
    </div>
  {:else if piece.kind === 'quote'}
    <div class="fam-quote">{piece.text}</div>
  {:else if piece.kind === 'tag'}
    <div class="fam-tag">{piece.text}</div>
  {:else if piece.kind === 'warnline'}
    <div class="fam-warnline">
      <span class="fam-lb">{piece.label}</span>
      <span>{piece.text}</span>
    </div>
  {:else if piece.kind === 'empty'}
    <div class="fam-empty">
      <Icon name={glyph} class="fam-empty-glyph" />
      {piece.text}
    </div>
  {:else if piece.kind === 'list'}
    <div class="fam-list">
      {#each piece.items as item, itemAt (itemAt)}
        <div class="fam-item">
          {#if item.id !== null}<span class="fam-id">{item.id.slice(0, 6)}</span>{/if}
          {#if item.state !== null}
            <span class="fam-state {item.state.tone}">{item.state.text}</span>
          {/if}
          <span class="fam-sub">{item.text}</span>
          {#if item.tag !== null}<span class="fam-tag">{item.tag}</span>{/if}
          {#if item.when !== null}<span class="fam-when">{item.when}</span>{/if}
        </div>
      {/each}
    </div>
  {/if}
{/each}

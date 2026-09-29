<script lang="ts">
  import type { ProcessSnapshot } from '../wire';
  import { processTree, walkedNote, type ProcessNode } from '../view';
  import Section from './Section.svelte';

  /**
   * The processes section: claude's descendant tree, a row each.
   *
   * **The tree is a nested list rather than two spaces per level.** The
   * terminal indented a row with `&nbsp;&nbsp;` per depth inside its key cell,
   * which is a character run standing in for hierarchy: no rule could reach
   * it, it did not wrap, and a space is not a layout step. Here a process's
   * children are a list inside its own item, so the indent is the browser's
   * and a deep row wraps into the column it is given.
   */
  let { walk, now }: { walk: ProcessSnapshot; now: number } = $props();

  const roots = $derived(processTree(walk));
  const note = $derived(walkedNote(walk.scanned_at, now));

  const count = $derived(countOf(roots));

  function countOf(nodes: ProcessNode[]): number {
    return nodes.reduce((total, node) => total + 1 + countOf(node.children), 0);
  }
</script>

{#snippet branch(nodes: ProcessNode[])}
  <ul class="tree">
    {#each nodes as node (node.pid)}
      <li>
        <div class="kv">
          <span class="k">{node.headline}</span>
          <span class="v">{node.memory}{' \u{b7} '}{node.pid}</span>
        </div>
        {#if node.children.length > 0}
          {@render branch(node.children)}
        {/if}
      </li>
    {/each}
  </ul>
{/snippet}

<Section name="processes" icon="processes" summary={`${count}`}>
  {@render branch(roots)}
  <div class="note">{note}</div>
</Section>

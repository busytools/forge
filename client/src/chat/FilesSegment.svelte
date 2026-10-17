<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { files } from './files.svelte';

  /**
   * The files slip in the strip above the composer: the seat's whole tree,
   * and the door to the page that reads it - and nothing else.
   *
   * **Mandatory, by the ruling (Ved, 2026-10-17)**: every seat has a tree -
   * a worker's own worktree, the lead's checkout - so this slip draws for
   * every seat with a record, where the tree row beside it keeps its own
   * rule of staying away when nothing has moved.
   *
   * **The slip IS the door, with no panel** (Ved, 2026-10-17, second and
   * third rulings): a press opens the files page - never a hover dialog -
   * and the label names the tree it opens: `walk · files` for a seat in a
   * worktree of its own (the worktree's name, so the destination is never a
   * guess), plain `files` for the project's checkout, the browser's own mark
   * beside it and no count.
   */
  let { href = null }: { href?: string | null } = $props();

  const name = $derived(files.name());
  const label = $derived(name === null ? 'files' : `${name} · files`);
</script>

{#if files.anything()}
  <span class="sg-seg">
    {#if href !== null}
      <a
        class="sg-tog"
        {href}
        aria-label={name === null ? "the seat's own files" : `the seat's files, in ${name}`}
      >
        <Icon name="files" />
        <span>{label}</span>
      </a>
    {:else}
      <span class="sg-tog">
        <Icon name="files" />
        <span>{label}</span>
      </span>
    {/if}
  </span>
{/if}

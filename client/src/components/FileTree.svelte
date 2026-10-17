<script lang="ts">
  /**
   * One tree's rows, drawn: the review page's own markup, shared, so the
   * review page and the files page draw ONE tree rather than two copies of
   * a row. The derivation is the page's (`components/tree.ts`); this is the
   * drawing. A file row's trailing cells - the review page's stats and
   * comment pill, the files page's comment pill - come from the caller's
   * snippet, so nothing page-specific lives here.
   */
  import type { Snippet } from 'svelte';

  import type { TreeRow } from './tree';

  interface Props {
    rows: TreeRow<unknown>[];
    /** The heading's word ("Files") and the nav's own label ("files in this scope"). */
    heading: string;
    label: string;
    count: number;
    closed: ReadonlySet<string>;
    /** The row marked as where the reader is - the canvas's reading edge, or the open file. */
    here: string | null;
    /**
     * The row the page's keyboard is on, drawn with the tree's own mark. The
     * page owns the keys (the files page's arrows walk without a prior
     * focus); this component only says which row they are on.
     */
    active?: string | null;
    /** A path whose file was renamed onto it, marked by the caller's own rule. */
    renamed?: (path: string) => boolean;
    ondir: (path: string) => void;
    onfile: (path: string) => void;
    trailing?: Snippet<[TreeRow<unknown>]>;
  }

  let {
    rows,
    heading,
    label,
    count,
    closed,
    here,
    active = null,
    renamed,
    ondir,
    onfile,
    trailing,
  }: Props = $props();
</script>

<nav class="rv-files" aria-label={label}>
  <h2 class="rv-h">{heading} <span class="rv-n">{count}</span></h2>
  {#each rows as row (row.kind + ':' + row.path)}
    {#if row.kind === 'dir'}
      <button
        type="button"
        class="rv-dir"
        class:key={active === row.path}
        style="--d:{row.depth}"
        data-path={row.path}
        aria-expanded={!closed.has(row.path)}
        onclick={() => ondir(row.path)}
      >
        <span class="rv-arw" class:open={!closed.has(row.path)}>&#9656;</span>
        <span class="rv-dirname">{row.name}/</span>
        <span class="rv-dircount">{row.count}</span>
      </button>
    {:else}
      <button
        type="button"
        class="rv-frow"
        class:renamed={renamed !== undefined && renamed(row.path)}
        class:here={here === row.path}
        class:key={active === row.path}
        aria-current={here === row.path}
        style="--d:{row.depth}"
        data-path={row.path}
        title={row.path}
        onclick={() => onfile(row.path)}
      >
        <span class="rv-fpath">{row.name}</span>
        {#if trailing}
          {@render trailing(row)}
        {/if}
      </button>
    {/if}
  {/each}
</nav>

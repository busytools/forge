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
    /**
     * Whether a folder row is a control. False under a filter, where the
     * tree is a match list and a fold would hide what was searched for - a
     * row that announces a disclosure it cannot perform is worse than a
     * plain row.
     */
    foldable?: boolean;
    /**
     * A row's own glyph, drawn at 16px before its name. A caller that draws
     * no icons (the review page) passes none, and its rows stay as they were.
     */
    icon?: (row: TreeRow<unknown>) => string | null;
    /** The tree's own root: the project's name with its path beside it. */
    root?: { name: string; path: string; icon: string | null } | null;
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
    foldable = true,
    icon,
    root = null,
    ondir,
    onfile,
    trailing,
  }: Props = $props();
</script>

<nav class="rv-files" aria-label={label}>
  <h2 class="rv-h">{heading} <span class="rv-n">{count}</span></h2>
  {#if root !== null}
    <div class="rv-root">
      {#if root.icon !== null}
        <img class="rv-icon" src={root.icon} alt="" width="16" height="16" />
      {/if}
      <span class="rv-rootname">{root.name}</span>
      <span class="rv-rootpath" title={root.path}>{root.path}</span>
    </div>
  {/if}
  {#each rows as row (row.kind + ':' + row.path)}
    {@const glyph = icon?.(row) ?? null}
    {#if row.kind === 'dir' && foldable}
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
        {#if glyph !== null}
          <img class="rv-icon" src={glyph} alt="" width="16" height="16" />
        {/if}
        <span class="rv-dirname">{row.name}/</span>
        <span class="rv-dircount">{row.count}</span>
      </button>
    {:else if row.kind === 'dir'}
      <div
        class="rv-dir plain"
        class:key={active === row.path}
        style="--d:{row.depth}"
        data-path={row.path}
      >
        <span class="rv-arw" aria-hidden="true"></span>
        {#if glyph !== null}
          <img class="rv-icon" src={glyph} alt="" width="16" height="16" />
        {/if}
        <span class="rv-dirname">{row.name}/</span>
        <span class="rv-dircount">{row.count}</span>
      </div>
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
        <span class="rv-fname">
          {#if glyph !== null}
            <img class="rv-icon" src={glyph} alt="" width="16" height="16" />
          {/if}
          <span class="rv-fpath">{row.name}</span>
        </span>
        {#if trailing}
          {@render trailing(row)}
        {/if}
      </button>
    {/if}
  {/each}
</nav>

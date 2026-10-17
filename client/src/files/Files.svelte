<script lang="ts">
  /**
   * The files page: one seat's whole working tree, walked file by file.
   *
   * It subscribes the seat itself, as the review page does, and answers
   * nothing. **The tree follows the SEAT** - a worker's own worktree, the
   * lead's checkout - so the page is keyed to the slot and never to a
   * branch the tree happens to be on.
   *
   * Its states are drawn here rather than left to a neighbour: nothing yet
   * (reading), a seat the server refused (its own sentence), and a tree
   * with no files. The reader and the comments rail land in this page's own
   * later commits of the same cut.
   */
  import { SvelteSet } from 'svelte/reactivity';

  import FileTree from '../components/FileTree.svelte';
  import { toggleClosed, type TreeRow } from '../components/tree';
  import { filesFrom } from '../composer/wire';
  import { hrefForSlot } from '../routes';
  import { watchSession, type SessionRead } from '../session/live';
  import type { SessionRecord } from '../session/wire';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { indexTreeRows, matchedEntries } from './tree';

  let { connection, slot }: { connection: Connection; slot: SessionSlot } = $props();

  let read = $state<SessionRead>({ wire: null, refused: null });

  $effect(() => {
    return watchSession(connection, slot, false).subscribe(($next) => {
      read = $next;
    });
  });

  const record: SessionRecord | null = $derived(read.wire);
  /** The seat's whole index; a floor for a record whose read has no files. */
  const entries = $derived(record === null ? [] : filesFrom(record.file_index));

  let query = $state('');
  const closed = new SvelteSet<string>();
  /** The file the reader shows, which the tree marks as current. */
  let open = $state<string | null>(null);

  const shown = $derived(matchedEntries(entries, query));
  const tree = $derived(indexTreeRows(shown, closed));

  /** Comments dropped on one file, from the seat-tree half - the row's pill. */
  function droppedCount(path: string): number {
    const held = record?.tree_reviews.threads;
    if (held?.outcome !== 'read') return 0;
    return held.value.filter((thread) => thread.anchor.path === path).length;
  }
</script>

<div class="files">
  <header class="files-top">
    <a class="files-back" href={hrefForSlot(slot)}>&lt;- session</a>
    <h1>Files</h1>
    {#if record !== null}
      <span class="files-facts">
        {record.state.scan_cwd}<span class="files-sep">|</span>{entries.length} files
      </span>
    {/if}
  </header>

  {#if read.refused !== null}
    <main class="files-main"><p class="files-quiet">{read.refused}</p></main>
  {:else if record === null}
    <main class="files-main"><p class="files-quiet">Reading the seat...</p></main>
  {:else if entries.length === 0}
    <main class="files-main"><p class="files-quiet">The seat's tree has no files.</p></main>
  {:else}
    <div class="files-body">
      <div class="files-side">
        <input
          class="files-filter"
          type="text"
          placeholder="filter files"
          aria-label="filter files"
          data-editor="nowhere"
          bind:value={query}
        />
        {#if tree.length === 0}
          <p class="files-quiet">No file matches this filter.</p>
        {:else}
          <FileTree
            rows={tree}
            heading="Tree"
            label="the seat's tree"
            count={entries.length}
            {closed}
            here={open}
            ondir={(path) => toggleClosed(closed, path)}
            onfile={(path) => (open = path)}
          >
            {#snippet trailing(row: TreeRow<unknown>)}
              {@const dropped = droppedCount(row.path)}
              {#if dropped > 0}
                <span class="rv-fcount"><span class="rv-ccnum">{dropped}</span></span>
              {/if}
            {/snippet}
          </FileTree>
        {/if}
      </div>
      <main class="files-main" aria-label="the file">
        {#if open === null}
          <p class="files-quiet">Pick a file from the tree.</p>
        {:else}
          <p class="files-quiet">Reading {open}...</p>
        {/if}
      </main>
    </div>
  {/if}
</div>

<style>
  /* The page's own pieces, scoped here as the review page's are: the sheet
     carries only what the pages share (the tree). */
  .files {
    display: flex;
    flex-direction: column;
    height: 100dvh;
    padding: 22px 26px 0;
  }
  .files-top {
    display: flex;
    align-items: baseline;
    gap: 14px;
    flex-wrap: wrap;
    padding-bottom: 14px;
    border-bottom: 1px solid var(--line);
  }
  .files-back {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
    text-decoration: none;
  }
  .files-back:hover {
    color: var(--text);
  }
  h1 {
    font-size: var(--fs-title);
    font-weight: 650;
    letter-spacing: -0.01em;
  }
  .files-facts {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
  }
  .files-sep {
    color: var(--line);
    margin: 0 7px;
  }
  .files-body {
    display: grid;
    grid-template-columns: minmax(0, 300px) minmax(0, 1fr);
    gap: 18px;
    flex: 1;
    min-height: 0;
    padding: 12px 0 0;
  }
  .files-side {
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .files-filter {
    background: none;
    border: 0;
    border-bottom: 1px solid var(--line);
    color: var(--text);
    font-family: var(--mono);
    font-size: var(--fs-label);
    padding: 4px 2px 8px;
    margin-bottom: 8px;
  }
  .files-main {
    min-height: 0;
    overflow: auto;
  }
  .files-quiet {
    font-size: var(--fs-base);
    color: var(--dim);
  }
  .files-main .files-quiet {
    margin-top: 12vh;
    text-align: center;
  }
  /* Narrow: the tree stacks over the reader, the page the one scroller. */
  @media (max-width: 980px) {
    .files {
      height: auto;
      min-height: 100dvh;
      padding: 18px 16px 0;
    }
    .files-body {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  @media (pointer: coarse) {
    .files-filter {
      min-height: 44px;
    }
  }
</style>

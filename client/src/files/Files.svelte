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
  import Reader from './Reader.svelte';
  import type { ReaderHandle } from './reader';
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
  /** The live reader, so the rail's jump-backs can reach its lines. */
  let reader = $state<ReaderHandle | null>(null);

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
          <Reader {connection} {slot} path={open} bind:handle={reader} />
        {/if}
      </main>
    </div>
  {/if}
</div>

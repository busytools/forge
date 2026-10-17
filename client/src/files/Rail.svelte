<script lang="ts">
  /**
   * The dropped-comments rail: what the reader wrote, as one list, and the
   * finish row that seals the sitting.
   *
   * A row is a jump back to its own line - the reader opens the file and the
   * page puts the caret there - and it wears the thread's own state chip and
   * captured first words, so the list reads without opening anything. A
   * thread parked (outdated, or no landing carried) says so on its row
   * rather than vanishing.
   *
   * The finish row is the review page's own writing (`finishRound`): a
   * `submit_review` for the TREE scope that seals every unfiled turn in one
   * round, and the seat hears `Review #N ready` exactly as it does for a
   * branch review.
   */
  import Box from './Box.svelte';
  import type { RailRow, Shelf } from './comments';
  import { statusName } from '../review/view';

  let {
    rows,
    repliesToRead,
    finishOpen = $bindable(false),
    canFinish,
    busy,
    onjump,
    onfinish,
  }: {
    rows: RailRow[];
    /** Threads the worker has answered and nobody read yet, the header's own count. */
    repliesToRead: number;
    finishOpen?: boolean;
    /** A round is worth sealing: at least one unfiled turn on the page. */
    canFinish: boolean;
    busy: boolean;
    onjump: (row: RailRow) => void;
    onfinish: (summary: string | null) => void;
  } = $props();

  /** The finish box's words, held here so the row's collapse does not take
   *  them, and the write-in-flight mirrored from the page. */
  const shelf = $state<Shelf>({ draft: '', busy: false });
  $effect(() => {
    shelf.busy = busy;
  });
</script>

<aside class="files-rail" aria-label="comments dropped">
  <h2 class="files-railhead">
    Comments <span class="files-railen">{rows.length}</span>
    {#if repliesToRead > 0}
      <span class="files-replies">{repliesToRead} to read</span>
    {/if}
  </h2>
  {#if rows.length === 0}
    <p class="files-quiet">Nothing dropped yet. Press a line to comment.</p>
  {:else}
    {#each rows as row (row.thread.id)}
      <button class="files-crow" type="button" onclick={() => onjump(row)}>
        <span class="rv-st {statusName(row.thread.status)}">{statusName(row.thread.status)}</span>
        <span class="files-cwhere">
          {row.thread.anchor.path}:{row.line ?? row.thread.anchor.line}
        </span>
        <span class="files-cwords">
          {row.thread.comments[0]?.text ?? ''}
        </span>
        {#if row.stale}
          <span class="files-cnote">the code moved under this - captured lines kept</span>
        {/if}
      </button>
    {/each}
  {/if}
  <div class="files-finish">
    {#if !finishOpen}
      <button
        class="rv-a on"
        type="button"
        disabled={!canFinish || busy}
        onclick={() => (finishOpen = true)}>finish review ({rows.length})</button
      >
      <div class="files-finishsub">
        seals the sitting into a round; the seat hears Review #N ready.
      </div>
    {:else}
      <Box
        class="rv-thread comment"
        headline="finish review"
        placeholder="what is this round about?"
        label="the round's summary"
        rows={2}
        sendLabel="seal the round"
        box={shelf}
        onsend={() => onfinish(shelf.draft.trim() === '' ? null : shelf.draft.trim())}
        oncancel={() => (finishOpen = false)}
      />
    {/if}
  </div>
</aside>

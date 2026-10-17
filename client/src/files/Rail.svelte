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
   * branch review. Its box is the page's `WordsBox` over the page's take,
   * so the summary dictates like every other box (Ved's hard rule).
   */
  import type { Voice, Words } from '../review/boxes';
  import RoundFinish from '../review/RoundFinish.svelte';
  import ThreadDelete from '../review/ThreadDelete.svelte';
  import { statusName } from '../review/view';
  import type { RailRow } from './comments';

  let {
    rows,
    repliesToRead,
    finishCount,
    busy,
    summaryBox,
    voice,
    onjump,
    onfinish,
    onpress,
    onhold,
    onfield,
    ondelete = null,
  }: {
    rows: RailRow[];
    /** Threads the worker has answered and nobody read yet, the header's own count. */
    repliesToRead: number;
    /** The unfiled turns a round would seal: the button's own count, and
     *  whether it is live at all. */
    finishCount: number;
    busy: boolean;
    /** The summary box, held on the page so a take's words land in it. */
    summaryBox: Words;
    voice: Voice;
    onjump: (row: RailRow) => void;
    onfinish: (text: string | null) => void;
    onpress: () => void;
    onhold: () => void;
    onfield: (el: HTMLElement | null) => void;
    /** Remove a row's thread; absent where the page offers no delete. */
    ondelete?: ((row: RailRow) => void | Promise<void>) | null;
  } = $props();

  /** The seal itself: the summary as written, or none. */
  function seal(): void {
    if (finishCount === 0 || busy) return;
    onfinish(summaryBox.draft.trim() === '' ? null : summaryBox.draft.trim());
  }
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
      <div class="files-crowwrap">
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
        {#if ondelete !== null}
          <ThreadDelete {busy} ondelete={() => ondelete(row)} />
        {/if}
      </div>
    {/each}
  {/if}
  <!-- The finish, drawn by the piece the review page draws too. -->
  <div class="files-finish">
    <RoundFinish
      bind:draft={summaryBox.draft}
      phase={summaryBox.phase}
      take={summaryBox.take}
      wire={summaryBox.wire}
      silent={summaryBox.silent}
      line={summaryBox.line}
      panel={summaryBox.panel}
      {voice}
      count={finishCount}
      complete={finishCount > 0}
      {busy}
      onfinish={seal}
      oncancel={() => voice.dictate.act('cancel')}
      {onpress}
      {onhold}
      {onfield}
    />
  </div>
</aside>

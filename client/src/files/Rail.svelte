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
  import { statusName } from '../review/view';
  import WordsBox from '../review/WordsBox.svelte';
  import type { RailRow } from './comments';

  let {
    rows,
    repliesToRead,
    finishCount,
    finishOpen = $bindable(false),
    busy,
    summaryBox,
    voice,
    onjump,
    onfinish,
    onpress,
    onhold,
    onfield,
  }: {
    rows: RailRow[];
    /** Threads the worker has answered and nobody read yet, the header's own count. */
    repliesToRead: number;
    /** The unfiled turns a round would seal: the button's own count, and
     *  whether it is live at all. */
    finishCount: number;
    finishOpen?: boolean;
    busy: boolean;
    /** The summary box, held on the page so a take's words land in it. */
    summaryBox: Words;
    voice: Voice;
    onjump: (row: RailRow) => void;
    onfinish: (text: string | null) => void;
    onpress: () => void;
    onhold: () => void;
    onfield: (el: HTMLElement | null) => void;
  } = $props();
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
        disabled={finishCount === 0 || busy}
        onclick={() => (finishOpen = true)}>finish review ({finishCount})</button
      >
      <div class="files-finishsub">
        {#if finishCount === 0}
          comment first; sealing happens when there is something to seal.
        {:else}
          seals the sitting into a round; the seat hears Review #N ready.
        {/if}
      </div>
    {:else}
      <WordsBox
        class="rv-thread comment"
        headline="finish review"
        placeholder="what is this round about?"
        label="the round's summary"
        rows={2}
        sendLabel="seal the round"
        box={summaryBox}
        {voice}
        onsend={(text: string) => onfinish(text)}
        oncancel={() => (finishOpen = false)}
        {onpress}
        {onhold}
        field={onfield}
      />
    {/if}
  </div>
</aside>

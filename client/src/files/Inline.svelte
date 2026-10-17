<script lang="ts">
  /**
   * One thread, drawn under its line in the reader: the state chip, where it
   * is, its turns, and its acts - over the review page's own pieces
   * (`ThreadTurn`, the chip and mark classes), so a thread reads the same
   * wherever it is read.
   *
   * The reply box is the page's `Box` and the page's own reply state: this
   * component draws what it is handed and reports presses.
   */
  import ThreadTurn from '../review/ThreadTurn.svelte';
  import { statusName } from '../review/view';
  import type { ReviewSetRecord } from '../session/wire';
  import Box from './Box.svelte';
  import type { RailRow, Shelf } from './comments';

  let {
    row,
    rounds,
    now,
    busy,
    replying,
    replyBox,
    onrespond,
    onreply,
    onresolve,
    onreopen,
  }: {
    row: RailRow;
    /** Every round of the seat, for the one a turn names. */
    rounds: ReviewSetRecord[];
    /** The clock the age chips read against. */
    now: number;
    /** A write is in flight on this page: the acts disable. */
    busy: boolean;
    replying: boolean;
    /** The page's reply shelf, drawn when `replying`. */
    replyBox: Shelf;
    onrespond: () => void;
    onreply: (text: string) => void;
    onresolve: () => void;
    onreopen: () => void;
  } = $props();

  const status = $derived(statusName(row.thread.status));
  const where = $derived(
    row.line === null
      ? `${row.thread.anchor.path.split('/').pop() ?? ''} · line ${row.thread.anchor.line}`
      : `${row.thread.anchor.path.split('/').pop() ?? ''} · line ${row.line}`,
  );
</script>

<article class="rv-thread compact">
  <div class="rv-tops">
    <span class="rv-mk {status}" aria-hidden="true"></span>
    <span class="rv-st {status}">{status}</span>
    <span class="rv-where">{where}</span>
    {#if row.movedFrom !== null}
      <span class="rv-moved">moved from {row.movedFrom}</span>
    {/if}
    {#if row.stale}
      <span class="rv-moved">parked - the code moved under it</span>
    {/if}
  </div>
  {#each row.thread.comments as turn, index (index)}
    <ThreadTurn {turn} {rounds} {index} {now} />
  {/each}
  <div class="rv-acts">
    <button class="rv-a" type="button" disabled={busy} onclick={onrespond}>respond</button>
    {#if status === 'resolved'}
      <button class="rv-a" type="button" disabled={busy} onclick={onreopen}>reopen</button>
    {:else}
      <button class="rv-a" type="button" disabled={busy} onclick={onresolve}>resolve</button>
    {/if}
  </div>
  {#if replying}
    <div class="rv-reply">
      <Box
        placeholder="Respond to this thread..."
        label="your response"
        rows={2}
        sendLabel="send"
        box={replyBox}
        onsend={onreply}
        oncancel={onrespond}
      />
    </div>
  {/if}
</article>

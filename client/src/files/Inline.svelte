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
  import type { ReplyBox, TurnEditor, Voice } from '../review/boxes';
  import ResolvedMarker from '../review/ResolvedMarker.svelte';
  import ThreadDelete from '../review/ThreadDelete.svelte';
  import ThreadTurn from '../review/ThreadTurn.svelte';
  import { statusName } from '../review/view';
  import WordsBox from '../review/WordsBox.svelte';
  import type { ReviewSetRecord } from '../session/wire';
  import type { RailRow } from './comments';

  let {
    row,
    rounds,
    now,
    busy,
    replying,
    replyBox,
    voice,
    editor = null,
    onrespond,
    onreply,
    onresolve,
    onreopen,
    ondelete = null,
    opened,
    onopen,
    oncollapse,
    onpress,
    onhold,
    field = () => {},
  }: {
    row: RailRow;
    /** Every round of the seat, for the one a turn names. */
    rounds: ReviewSetRecord[];
    /** The clock the age chips read against. */
    now: number;
    /** A write is in flight on this page: the acts disable. */
    busy: boolean;
    replying: boolean;
    /** The page's reply box, drawn when `replying`. */
    replyBox: ReplyBox;
    /** What dictation means to this page - every box wears the mic. */
    voice: Voice;
    /** The page's edit box, handed to every turn this card draws. */
    editor?: TurnEditor | null;
    onrespond: () => void;
    onreply: (text: string) => void;
    onresolve: () => void;
    onreopen: () => void;
    /** Remove this thread; absent where the page offers no delete. */
    ondelete?: (() => void | Promise<void>) | null;
    /** The resolved threads the page holds open: the state outlives this
     *  card, which CM rebuilds whenever a write moves the block key. */
    opened: ReadonlySet<string>;
    onopen: (thread: string) => void;
    oncollapse: (thread: string) => void;
    onpress: () => void;
    onhold: () => void;
    /** The field, so the page can put the keyboard back in it. */
    field?: (el: HTMLElement | null) => void;
  } = $props();

  const status = $derived(statusName(row.thread.status));
  const where = $derived(
    row.line === null
      ? `${row.thread.anchor.path.split('/').pop() ?? ''} · line ${row.thread.anchor.line}`
      : `${row.thread.anchor.path.split('/').pop() ?? ''} · line ${row.line}`,
  );

  /**
   * A resolved thread collapses to its marker line, the way the review
   * page's canvas does (Ved, 2026-10-17) - and is opened by a press on it.
   * Reopening un-resolves the thread, which expands it again by itself.
   *
   * **The open state is the PAGE's, not this card's**: a write moves the
   * block key and CodeMirror tears the card down and builds it again, so a
   * locally-held `opened` folded the thread back before its own edit box
   * could draw (the lead's gate, 2026-10-17).
   */
  const collapsed = $derived(status === 'resolved' && !opened.has(row.thread.id));
</script>

{#if collapsed}
  <ResolvedMarker
    first={row.thread.comments[0]?.text ?? ''}
    {where}
    onopen={() => onopen(row.thread.id)}
  />
{:else}
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
      <ThreadTurn {turn} {rounds} {index} {now} thread={row.thread.id} {editor} />
    {/each}
    <div class="rv-acts">
      {#if status === 'resolved'}
        <!-- A resolved thread's acts are the open hand-back and the fold
             (Ved, 2026-10-17): no respond on something accepted, and one
             way back to the marker it came from. -->
        <button class="rv-a" type="button" disabled={busy} onclick={onreopen}>reopen</button>
        <button class="rv-a" type="button" onclick={() => oncollapse(row.thread.id)}
          >collapse</button
        >
      {:else}
        <button class="rv-a" type="button" disabled={busy} onclick={onrespond}>respond</button>
        <button class="rv-a" type="button" disabled={busy} onclick={onresolve}>resolve</button>
      {/if}
      {#if ondelete !== null}
        <ThreadDelete {busy} {ondelete} />
      {/if}
    </div>
    {#if replying}
      <div class="rv-reply">
        <WordsBox
          class="rv-replybox rv-thread compact"
          placeholder="Respond to this thread..."
          label="your response"
          rows={2}
          sendLabel="send"
          box={replyBox}
          {voice}
          onsend={onreply}
          oncancel={onrespond}
          {onpress}
          {onhold}
          {field}
        />
      </div>
    {/if}
  </article>
{/if}

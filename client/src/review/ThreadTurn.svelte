<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
  import type { TurnEditor } from './boxes';
  import WordsBox from './WordsBox.svelte';
  import { ageOf, authorName, turnRound } from './view';

  /**
   * One turn of a thread: who wrote it, the one chip that dates it, and its
   * words.
   *
   * **Both halves of the page draw this**, so a turn reads the same wherever
   * it is read: the conversation renders it in its template, and the canvas
   * mounts it into the rows it builds for the diff. The words go through the
   * chat's own renderer rather than a second one, and the chip is an element
   * the sheet boxes - never a tag welded onto the name.
   *
   * **The chip is the round that sealed the turn** (only a reviewer's turn
   * files into one), else the turn's age from the second turn on: the
   * thread's own opening turn has nothing to be later than. An unparsable
   * stamp draws no chip rather than a confident `0m`.
   *
   * **A turn the reader wrote takes an `edit` act where the surface offers
   * one** (Ved, 2026-10-17: comments are editable everywhere they are
   * drawn): the box opens under the turn with its own words in it, and the
   * words ride the page's reply slot - one box, one take, one landing - so
   * the mic works here exactly as in every other box.
   */
  let {
    turn,
    rounds,
    index,
    now,
    thread,
    editor = null,
  }: {
    turn: ReviewThreadRecord['comments'][number];
    /** Every round of the seat, for the one a turn names. */
    rounds: ReviewSetRecord[];
    /** The turn's place in its thread. */
    index: number;
    /** The clock the age reads against. */
    now: number;
    /** The thread this turn belongs to, for the edit box's addressing. */
    thread: string;
    /** The page's edit box and its verbs; absent where editing is not offered. */
    editor?: TurnEditor | null;
  } = $props();

  const sealedInto = $derived(turnRound(rounds, turn));
  const stamp = $derived(
    sealedInto !== null ? `R${sealedInto}` : index > 0 ? ageOf(turn.at, now) : '',
  );

  /** Whether the page's box is editing THIS turn, which is where it draws. */
  const editing = $derived(
    editor !== null && editor.box.thread === thread && editor.box.index === index,
  );
  const canEdit = $derived(editor !== null && turn.author.kind === 'user');
</script>

<div class="rv-turn" class:you={turn.author.kind === 'user'}>
  <div class="rv-who">
    <span class="rv-nm">{authorName(turn.author)}</span>
    {#if stamp !== ''}<span class="rv-at">{stamp}</span>{/if}
    {#if canEdit && !editing}
      <button
        class="rv-a rv-edit"
        type="button"
        onclick={() => editor?.begin(thread, index, turn.text)}>edit</button
      >
    {/if}
  </div>
  <Prose text={turn.text} preserveLines />
  {#if editing && editor !== null}
    <div class="rv-turnedit">
      <WordsBox
        class="rv-reply"
        rows={2}
        placeholder="the comment's words..."
        label="edit this comment"
        sendLabel="save"
        box={editor.box}
        voice={editor.voice}
        onsend={(text: string) => editor?.send(thread, index, text)}
        oncancel={() => editor?.cancel()}
        onpress={() => editor?.press()}
        onhold={() => editor?.hold()}
        field={(el: HTMLElement | null) => editor?.field(el)}
      />
    </div>
  {/if}
</div>

<style>
  /* The edit act rides the who-row, small: it is a per-turn verb, not one of
     the card's. */
  .rv-edit {
    margin-left: auto;
    padding: 0 7px;
  }
  /* The box under its own turn, so the words being changed stay in sight. */
  .rv-turnedit {
    margin-top: 6px;
  }
</style>

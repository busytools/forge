<script lang="ts">
  import Prose from '../chat/Prose.svelte';
  import type { ReviewSetRecord, ReviewThreadRecord } from '../session/wire';
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
   */
  let {
    turn,
    rounds,
    index,
    now,
  }: {
    turn: ReviewThreadRecord['comments'][number];
    /** Every round of the seat, for the one a turn names. */
    rounds: ReviewSetRecord[];
    /** The turn's place in its thread. */
    index: number;
    /** The clock the age reads against. */
    now: number;
  } = $props();

  const sealedInto = $derived(turnRound(rounds, turn));
  const stamp = $derived(
    sealedInto !== null ? `R${sealedInto}` : index > 0 ? ageOf(turn.at, now) : '',
  );
</script>

<div class="rv-turn" class:you={turn.author.kind === 'user'}>
  <div class="rv-who">
    <span class="rv-nm">{authorName(turn.author)}</span>
    {#if stamp !== ''}<span class="rv-at">{stamp}</span>{/if}
  </div>
  <Prose text={turn.text} preserveLines />
</div>

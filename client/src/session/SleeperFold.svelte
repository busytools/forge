<script lang="ts">
  import type { Row } from '../home/view';
  import Chevron from '../components/Chevron.svelte';
  import type { SessionSlot } from '../wire/types';
  import { foldMark, rowMark } from './view';

  /**
   * A project's sleeping seats, behind one row that counts them.
   *
   * **The open state is held here rather than taken from the rail**, because
   * the rail redraws on every fleet update and a fold whose state came from
   * outside would close under the reader the moment one arrived. And it is
   * one-way: the fold OPENS when the seat the page is showing moves in behind
   * it - an occupant swap onto a sleeping seat reaches this without a fresh
   * mount - and never closes on its own.
   *
   * A sleeping row is information, not a way in: its seat has no session
   * behind it, so opening it draws a refusal, and the terminal's own answer
   * for these rows is a label with no hit target. So no link, and no close
   * chip - there is nothing behind either of them to act on.
   *
   * `closing` is the rail's own mark for a seat this client has just closed:
   * such a row lands here at once and keeps a settling dot while the core
   * finishes shutting it down (#1712).
   */
  let {
    sleeping,
    shown,
    closing,
  }: { sleeping: Row[]; shown: string | null; closing: (slot: SessionSlot) => boolean } = $props();

  /** What one sleeping row wears, which is what its fold's summary must too. */
  const markOf = (row: Row): string => rowMark(row, closing);

  const holds = () => sleeping.some((row) => row.slot.label === shown);
  // Read once, through a call: the initial value is the whole of what the
  // first render is owed, as the inspector's own sections do it.
  let open = $state(holds());
  $effect(() => {
    if (holds()) open = true;
  });
</script>

<details class="sfold" bind:open>
  <summary class="wk">
    <!-- The summary wears what the rows behind it wear, settling included:
         the fold is display, so the pulse cannot live only under it. -->
    <span class="dot {foldMark(sleeping.map(markOf)) ?? 'off'}"></span>
    <span class="nm">{sleeping.length} asleep</span>
    <Chevron />
  </summary>
  {#each sleeping as worker (worker.slot.label)}
    <div class="wk" class:on={shown === worker.slot.label}>
      <span class="dot {markOf(worker)}"></span>
      <span class="nm">{worker.slot.label}</span>
    </div>
  {/each}
</details>

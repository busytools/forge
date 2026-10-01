<script lang="ts">
  import type { Row } from '../home/view';
  import Chevron from '../components/Chevron.svelte';
  import { hrefForSlot } from '../routes';
  import CloseChip from './CloseChip.svelte';
  import { railMark } from './view';

  /**
   * A project's sleeping seats, behind one row that counts them.
   *
   * **The open state is held here rather than taken from the rail**, because
   * the rail redraws on every fleet update and a fold whose state came from
   * outside would close under the reader the moment one arrived. And it is
   * one-way: the fold OPENS when the seat the page is showing moves in behind
   * it - an occupant swap onto a sleeping seat reaches this without a fresh
   * mount - and never closes on its own.
   */
  let { sleeping, shown }: { sleeping: Row[]; shown: string | null } = $props();

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
    <span class="dot off"></span>
    <span class="nm">{sleeping.length} asleep</span>
    <Chevron />
  </summary>
  {#each sleeping as worker (worker.slot.label)}
    <div class="wk" class:on={shown === worker.slot.label}>
      <span class="dot {railMark(worker.state)}"></span>
      <span class="nm"><a href={hrefForSlot(worker.slot)}>{worker.slot.label}</a></span>
      <CloseChip />
    </div>
  {/each}
</details>

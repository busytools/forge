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
   * outside would close under the reader the moment one arrived. It STARTS
   * open when the seat the page is showing is one of the seats behind it: a
   * fold that hid the row a reader is looking at is the one thing it must
   * never do.
   */
  let { sleeping, shown }: { sleeping: Row[]; shown: string | null } = $props();

  // Read once, through a call: the initial value is the whole of what the
  // prop is for, as the inspector's own sections do it.
  const initially = () => sleeping.some((row) => row.slot.label === shown);
  let open = $state(initially());
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

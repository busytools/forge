<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import HookBatchBody from './HookBatchBody.svelte';
  import type { HookFootLeaf } from './units';

  /**
   * A turn's silent hook batches, as one row where the turn's first silent
   * stop fired.
   *
   * **The frame still draws; the cost is one row per turn instead of one per
   * stop** (#3054). Every stop of a long session runs the same few Stop hooks
   * - `abstract`'s learnings post and `code-modernization`'s telemetry pair
   * were the reported three - and each one drew a full disclosure row saying
   * nothing, so a review loop buried its turn in identical rows. The chip
   * counts the batches and lists every one of them behind its open, so a
   * reader who wants the commands and durations still has them (rule 25: a
   * frame drawn behind a disclosure is drawn, not dropped).
   *
   * **The chip keeps the place its first batch fired** (#3092): the rows the
   * turn gains after that stop draw below it rather than above it.
   *
   * A batch that carries anything is not here: it keeps its own row where it
   * fired.
   */
  let { foot, open = false }: { foot: HookFootLeaf; open?: boolean } = $props();

  /** Whether the chip is open; a closed one carries its count and nothing else. */
  let opened = $state(untrack(() => open));

  /** The batches, and the hooks they counted: what one glance has to say. */
  const label = $derived(
    `hooks \u{b7} ${foot.batches.length} ${foot.batches.length === 1 ? 'batch' : 'batches'}` +
      ` \u{b7} ${foot.actions} ${foot.actions === 1 ? 'hook' : 'hooks'}`,
  );
</script>

<details class="leaf hookfoot" bind:open={opened}>
  <summary>
    <!-- Silent batches are exactly the ones nothing went wrong in. -->
    <Icon name="hook" class="gl ok" />
    <span class="tn">{label}</span>
    <Chevron />
  </summary>
  {#if opened}
    <div class="body">
      {#each foot.batches as batch (batch.key)}
        <div class="bfoot">
          <HookBatchBody {batch} />
        </div>
      {/each}
    </div>
  {/if}
</details>

<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import HookBatchBody from './HookBatchBody.svelte';
  import { batchHasBody, type HookBatchLeaf } from './units';

  /**
   * One stop-hook batch that carries something, as its own row: the count and
   * any errors on the summary, every hook and its context behind the open.
   *
   * **Only a carrying batch is a row.** A silent one - the same few plugin
   * hooks on every stop, printing nothing - goes behind the turn foot's chip
   * (`HookFoot.svelte`), because a full disclosure row per stop is what filled
   * the view (#3054). A batch that reports output, an error, additional
   * context or a prevented continuation is what the reader is owed at a
   * glance, so this row stays.
   *
   * The frame sends none of these when no hook fired, so a row on the page
   * always has something behind it.
   */
  let { batch, open = false }: { batch: HookBatchLeaf; open?: boolean } = $props();

  /** Whether the row is open; a closed one carries its summary and nothing else. */
  let opened = $state(untrack(() => open));

  /** The summary's own words, errors counted on the closed row too. */
  const label = $derived(
    `hook summary \u{b7} ${batch.actions} ${batch.actions === 1 ? 'action' : 'actions'}` +
      (batch.errors.length > 0
        ? ` \u{b7} ${batch.errors.length} ${batch.errors.length === 1 ? 'error' : 'errors'}`
        : ''),
  );
</script>

<details class="leaf hookgroup hookbatch" bind:open={opened}>
  <summary>
    <!-- A batch only arrives once its hooks have settled, so the passing tone
         is true unless the CLI reported an error against it. -->
    <Icon name="hook" class={`gl${batch.errors.length > 0 ? ' err' : ' ok'}`} />
    <span class="tn">{label}</span>
    <Chevron />
  </summary>
  {#if opened && batchHasBody(batch)}
    <div class="body">
      <HookBatchBody {batch} />
    </div>
  {/if}
</details>

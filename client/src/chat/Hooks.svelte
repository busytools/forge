<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { duration } from './numbers';
  import type { HookBatchLeaf } from './units';

  /**
   * What a turn's hooks did: the count, one row per hook behind it, and the
   * errors the CLI reported against the batch.
   *
   * **The batch is a tool row, not a line of its own.** It is work the turn
   * ran, so it draws with the shape every call and hook row draws - the hook
   * glyph, the title, the chevron - and takes its place in the turn's tool
   * list where it fired rather than standing above the work (Ved, 2026-11-13).
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

  /**
   * How long one hook took, which is milliseconds where the row has them.
   *
   * Every hook in the captured corpus ran in under a second (3ms, 6ms, 412ms),
   * and the row's own drawing says `3ms`: the shared span formatter floors
   * those to `0.0s`, which is a duration drawn as no duration at all.
   */
  function took(ms: number): string {
    return ms < 1000 ? `${ms}ms` : duration(ms);
  }
</script>

<details class="leaf hookgroup hookbatch" bind:open={opened}>
  <summary>
    <!-- A batch only arrives once its hooks have settled, so the passing tone
         is true unless the CLI reported an error against it. -->
    <Icon name="hook" class={`gl${batch.errors.length > 0 ? ' err' : ' ok'}`} />
    <span class="tn">{label}</span>
    <Chevron />
  </summary>
  {#if opened && (batch.infos.length > 0 || batch.errors.length > 0)}
    <div class="body">
      {#each batch.infos as info, at (at)}
        <div class="term">
          {info.command}{#if info.durationMs !== undefined}{' \u{b7} '}{took(info.durationMs)}{/if}
        </div>
      {/each}
      {#each batch.errors as error, at (at)}
        <div class="term"><span class="fail">{error}</span></div>
      {/each}
    </div>
  {/if}
</details>

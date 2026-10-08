<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { duration } from './numbers';
  import type { HookInfo } from './units';

  /**
   * What a turn's hooks did: the count, one row per hook behind it, and the
   * errors the CLI reported against the batch.
   *
   * The frame sends none of these when no hook fired, so a chip on the page
   * always has something behind it.
   */
  let {
    actions,
    infos,
    errors,
    open = false,
  }: { actions: number; infos: HookInfo[]; errors: string[]; open?: boolean } = $props();

  /** Whether the chip is open; a closed one carries its summary and nothing else. */
  let opened = $state(untrack(() => open));

  /** The summary's own words, errors counted on the closed chip too. */
  const label = $derived(
    `hook summary \u{b7} ${actions} ${actions === 1 ? 'action' : 'actions'}` +
      (errors.length > 0
        ? ` \u{b7} ${errors.length} ${errors.length === 1 ? 'error' : 'errors'}`
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

<details class="hooks" bind:open={opened}>
  <summary>
    {#if errors.length > 0}
      <Icon name="x" class="st err" />
    {/if}
    <span>{label}</span>
    <Chevron />
  </summary>
  {#if opened && (infos.length > 0 || errors.length > 0)}
    <div class="body">
      {#each infos as info, at (at)}
        <div class="term">
          {info.command}{#if info.durationMs !== undefined}{' \u{b7} '}{took(info.durationMs)}{/if}
        </div>
      {/each}
      {#each errors as error, at (at)}
        <div class="term"><span class="fail">{error}</span></div>
      {/each}
    </div>
  {/if}
</details>

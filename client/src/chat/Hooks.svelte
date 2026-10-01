<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { duration } from './numbers';
  import type { HookInfo } from './units';

  /**
   * What a turn's hooks did: the count, and one row per hook behind it.
   *
   * The frame sends none of these when no hook fired, so a chip on the page
   * always has something behind it.
   */
  let { actions, infos }: { actions: number; infos: HookInfo[] } = $props();

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

<details class="hooks">
  <summary>
    <span>hook summary {'\u{b7}'} {actions} {actions === 1 ? 'action' : 'actions'}</span>
    <Chevron />
  </summary>
  {#if infos.length > 0}
    <div class="body">
      {#each infos as info, at (at)}
        <div class="term">
          {info.command}{#if info.durationMs !== undefined}{' \u{b7} '}{took(info.durationMs)}{/if}
        </div>
      {/each}
    </div>
  {/if}
</details>

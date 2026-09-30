<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { duration } from './numbers';
  import type { HookInfo } from './units';

  /**
   * What a turn's hooks did: the count, and one row per hook behind it.
   *
   * The frame sends none of these when no hook fired, so a chip on the page
   * always has something behind it.
   *
   * **The mark is the sprite's.** The chip led with a corner arrow because a
   * character cell was all a terminal could draw, and it sits beside real
   * icons on every other row here.
   */
  let { actions, infos }: { actions: number; infos: HookInfo[] } = $props();

  /**
   * Whether the chip is open, for the same reason the row's figures are here:
   * the disclosure's state is drawn, and a state a reader can see has to be
   * one a reader who cannot see it is told.
   */
  let open = $state(false);

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

<details class="hooks" bind:open>
  <summary>
    <Icon name="chev" class="hb" />
    <span>hook summary {'\u{b7}'} {actions} {actions === 1 ? 'action' : 'actions'}</span>
    <!-- The label is a text node rather than a `::after` rule: the CSS form
         leaves the disclosure's accessible name to whatever the user agent
         makes of generated content, and it is the toggle's own word here. -->
    <span class="tog">{open ? 'collapse' : 'expand'}</span>
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

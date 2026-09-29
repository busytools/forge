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
</script>

<details class="hooks">
  <summary>
    <Icon name="chev" class="hb" />
    <span>hook summary {'\u{b7}'} {actions} actions</span>
    <span class="tog"></span>
  </summary>
  {#if infos.length > 0}
    <div class="body">
      {#each infos as info, at (at)}
        <div class="term">
          {info.command}{#if info.durationMs !== undefined}{' \u{b7} '}{duration(
              info.durationMs,
            )}{/if}
        </div>
      {/each}
    </div>
  {/if}
</details>

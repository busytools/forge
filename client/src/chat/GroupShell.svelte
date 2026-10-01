<script lang="ts" generics="Row extends { key: string }">
  import type { Snippet } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import type { CallStatus } from './families';

  /**
   * One run of rows: the count and the run's own status, then a lane per kind
   * with each row behind a rail.
   *
   * **The header is the count and the aggregate, and nothing else.** A list of
   * the kinds it met is not wanted: the lanes below say which kinds ran, and a
   * summary that repeats them spends the row on a second naming.
   *
   * **The roll-up says the run has a failure in it, never which row.** Every
   * row carries its own status for that, and the group's is only the aggregate
   * - which is why a failed run is still a run a reader can read.
   *
   * **A tool run and a run of peer messages are the same disclosure**, and the
   * differences are all data: the noun, each lane's glyph and label, and what a
   * row draws. Sharing the shell is what keeps the mark honest in both - a
   * group that copies this markup is a group that can lose the status branch,
   * which is what a failed delivery drew as a green check from.
   *
   * **A row carries its own key, the way a lane does**, and the type says so:
   * a run keyed by position is remade whenever anything above it is inserted,
   * and a row is where a call's open state lives. The shell is generic over the
   * row, so it cannot know what identifies one - the caller can, and does, at
   * the point it builds the rows.
   */
  let {
    name,
    count,
    noun,
    status,
    lanes,
    leaf,
  }: {
    name: string;
    count: number;
    noun: string;
    status: CallStatus;
    lanes: { key: string; glyph: string; label: string; rows: Row[] }[];
    leaf: Snippet<[Row]>;
  } = $props();
</script>

<details class="kind" open data-k={name}>
  <summary>
    {#if status === 'completed'}
      <Icon name="check" class="st" />
    {:else if status === 'failed' || status === 'killed'}
      <Icon name="x" class="st err" />
    {:else}
      <span class="st"><span class="ring"></span></span>
    {/if}
    <span class="nm">{count} {noun}{count === 1 ? '' : 's'}</span>
    <Chevron />
  </summary>
  <div class="leaves">
    {#each lanes as lane (lane.key)}
      <div class="knd">
        <Icon name={lane.glyph} class="gl" />
        <span class="nm">{lane.label}</span>
      </div>
      {#each lane.rows as row (row.key)}
        {@render leaf(row)}
      {/each}
    {/each}
  </div>
</details>

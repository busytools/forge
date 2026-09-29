<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { iconOf, type CallStatus } from './families';
  import Call from './Call.svelte';
  import { opensByDefault, type ToolLeaf } from './leaves';
  import type { FamilyLeaves } from './units';

  /**
   * One run of consecutive calls: the count and the run's own status, then a
   * label per family with its calls on their own lane behind a rail.
   *
   * **The header is the count and the aggregate, and nothing else.** A list of
   * the kinds it met is not wanted: the lanes below say which families ran,
   * and a summary that repeats them spends the row on a second naming.
   *
   * **The roll-up says the run has a failure in it, never which call.** Every
   * call carries its own status for that, and the group's is only the
   * aggregate - which is why a failed run is still a run a reader can read.
   */
  let { families, status }: { families: FamilyLeaves[]; status: CallStatus } = $props();

  const calls = $derived(families.reduce((total, family) => total + family.calls.length, 0));

  /**
   * What names the run, which is what a view keys its open state on.
   *
   * The call the run OPENED with: the calls after it are appended, so the
   * thing that identifies the run does not move as they arrive.
   */
  const key = $derived(`group-${families[0]?.calls[0]?.id ?? 'empty'}`);

  /** Whether a call's body is drawn without being asked for: a mutation's diff. */
  function opens(call: ToolLeaf): boolean {
    return opensByDefault(call.name);
  }
</script>

<details class="kind" open data-k={key}>
  <summary>
    {#if status === 'completed'}
      <Icon name="check" class="st" />
    {:else if status === 'failed' || status === 'killed'}
      <Icon name="x" class="st err" />
    {:else}
      <span class="st"><span class="ring"></span></span>
    {/if}
    <span class="nm">{calls} tool {calls === 1 ? 'call' : 'calls'}</span>
    <Chevron />
  </summary>
  <div class="leaves">
    {#each families as family (family.label)}
      <div class="knd">
        <Icon name={iconOf(family.row)} class="gl" />
        <span class="nm">{family.label}</span>
      </div>
      {#each family.calls as call (call.id)}
        <Call {call} open={opens(call)} />
      {/each}
    {/each}
  </div>
</details>

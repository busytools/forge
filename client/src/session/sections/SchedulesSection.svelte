<script lang="ts">
  import type { CronEntry } from '../../wire/home';
  import { schedulesSection } from '../view';
  import Section from './Section.svelte';

  /**
   * The schedules section: the durable crons that fire into this project, with
   * when each one is next due.
   */
  let { crons, now }: { crons: CronEntry[]; now: number } = $props();

  const view = $derived(schedulesSection(crons, now));
</script>

<Section name="schedules" icon="schedules" summary={view.summary}>
  {#each view.rows as row (row.k)}
    <div class="kv"><span class="k">{row.k}</span><span class="v">{row.v}</span></div>
  {/each}
</Section>

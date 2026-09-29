<script lang="ts">
  import type { Task } from '../../wire/home';
  import { tasksSection } from '../view';
  import Section from './Section.svelte';

  /** The tasks section: what the project holds, in the order a person reads them. */
  let { tasks }: { tasks: Task[] } = $props();

  const view = $derived(tasksSection(tasks));
</script>

<Section name="tasks" icon="tasks" summary={view.summary}>
  {#each view.rows as row (row.subject + (row.owner ?? ''))}
    <div class={row.klass}>
      <span class="b"></span>
      <span>
        <div class="s">{row.subject}</div>
        <div class="meta">
          {#if row.owner !== null}<b>{row.owner}</b>{' \u{b7} '}{/if}{row.meta}
        </div>
      </span>
    </div>
  {/each}
</Section>

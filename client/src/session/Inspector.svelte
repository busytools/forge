<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import GitSection from './sections/GitSection.svelte';
  import GotifySection from './sections/GotifySection.svelte';
  import McpSection from './sections/McpSection.svelte';
  import MonitorsSection from './sections/MonitorsSection.svelte';
  import ProcessesSection from './sections/ProcessesSection.svelte';
  import SchedulesSection from './sections/SchedulesSection.svelte';
  import SlackSection from './sections/SlackSection.svelte';
  import TasksSection from './sections/TasksSection.svelte';
  import { gitSection, gotifySection, mcpSection, projectOf, slackSection } from './view';
  import type { SessionRecord } from './wire';

  /**
   * The inspector: one section per subject, each collapsed to a name and a
   * summary and opening in place.
   *
   * **A section with nothing behind it is not drawn.** That is the server's
   * own rule, and it is why a fresh seat's page looks short: a section that is
   * always there says nothing when it is empty. The git section leads, because
   * the working tree is the first thing a person looks for and it is the one
   * read whose absence used to read as "this seat has no repository".
   *
   * A record that has not arrived yet draws no section rather than an empty
   * one, which is the same shape as a seat with nothing to say.
   */
  let {
    wire,
    record,
    slot,
    now,
    onclose,
  }: {
    wire: HomeWire;
    record: SessionRecord | null;
    slot: SessionSlot;
    now: number;
    /** Brings the header's handle back, which the folded inspector covers. */
    onclose: () => void;
  } = $props();

  const project = $derived(projectOf(wire, slot));
  const git = $derived(record === null ? null : gitSection(record));

  const tasks = $derived(project?.tasks ?? []);
  const crons = $derived(project?.crons ?? []);
  // The sections' subscriptions come off the seat's own row, like its tasks
  // and crons; the connector liveness they draw beside them is home-wide.
  const gotify = $derived(gotifySection(wire, project));
  const slack = $derived(slackSection(wire, project));
  const mcp = $derived(record === null ? null : mcpSection(record));
  const walk = $derived(record?.processes ?? null);
  const monitors = $derived(record?.monitors ?? []);
</script>

<!-- Named for the same reason the rail is: the page carries two `aside`
     elements, and two complementary landmarks sharing an implicit name are
     one landmark to a screen reader. -->
<aside class="rail right" aria-label="inspector">
  <div class="banner">
    <span class="t">inspector</span>
    <span class="n ml">{slot.project}</span>
    <button
      class="close"
      type="button"
      title="close"
      aria-label="close the inspector"
      onclick={onclose}
    >
      <Icon name="x" />
    </button>
  </div>
  <div class="scroll">
    {#if git !== null}
      <GitSection view={git} />
    {/if}
    {#if tasks.length > 0}
      <TasksSection {tasks} />
    {/if}
    {#if crons.length > 0}
      <SchedulesSection {crons} {now} />
    {/if}
    {#if gotify !== null}
      <GotifySection view={gotify} />
    {/if}
    {#if slack !== null}
      <SlackSection view={slack} />
    {/if}
    {#if mcp !== null}
      <McpSection view={mcp} />
    {/if}
    {#if walk !== null && walk.processes.length > 0}
      <ProcessesSection {walk} {now} />
    {/if}
    {#if monitors.length > 0}
      <MonitorsSection {monitors} {now} />
    {/if}
  </div>
</aside>

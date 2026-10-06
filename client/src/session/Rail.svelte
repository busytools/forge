<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { hrefForSlot } from '../routes';
  import type { Connection } from '../socket';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import CloseChip from './CloseChip.svelte';
  import { closeSeat, closingSeat } from './close';
  import GroupFold from './GroupFold.svelte';
  import SleeperFold from './SleeperFold.svelte';
  import {
    failedLine,
    fleetCount,
    railFooter,
    railGroups,
    railMark,
    type RailProject,
  } from './view';

  /**
   * The projects rail: every declared project, grouped by the strongest state
   * among its own rows, with its workers under it.
   *
   * The rows are the home's, so a seat reads the same here as it does one
   * click away. What the rail adds is the grouping and the reason line.
   */
  let {
    home,
    current,
    now,
    connection,
    onclose,
  }: {
    home: HomeWire;
    /** The seat the page is showing, which is the one row the rail marks. */
    current: SessionSlot;
    now: number;
    /** What a row's close chip acts through, and what moves the reader after it. */
    connection: Connection;
    /** Brings the header's handle back, which the collapsed rail has covered. */
    onclose: () => void;
  } = $props();

  const groups = $derived(railGroups(home, current, now, closingSeat));
  /**
   * The account and the two builds, which sit under the list rather than in it:
   * the projects scroll behind them.
   */
  const foot = $derived(railFooter(home, current));

  /**
   * Close one row's seat. The command the row sends and where the reader
   * lands are `session/close.ts`'s, which knows the terminal's own two
   * gestures apart.
   */
  function close(slot: SessionSlot): void {
    closeSeat(connection, home, slot, current, now);
  }
</script>

<!--
  Named, because the page carries two `aside` elements: two complementary
  landmarks with the same implicit name are one landmark to a screen reader,
  and neither can be navigated to by name.
-->
<aside class="rail left" aria-label="projects">
  <div class="banner">
    <span class="t">projects</span>
    <span class="n ml">{fleetCount(home)}</span>
    <!-- A rail covering the page carries its own way out: the header handle
         that opened it is underneath. -->
    <button
      class="close"
      type="button"
      title="close"
      aria-label="close the projects rail"
      onclick={onclose}
    >
      <Icon name="x" />
    </button>
  </div>
  <div class="scroll">
    <!-- One project's block, drawn the same in a folded group and an open
         one: the asleep heading is the only thing a folded group changes. -->
    {#snippet projectBlock(project: RailProject)}
      {@const leadDot = closingSeat(project.row.slot)
        ? 'off settling'
        : railMark(project.row.state)}
      <!-- A row this client has just closed reads asleep AT ONCE, and its dot
           pulses while the core finishes: the seat leaves the reader's
           working section on the click rather than sitting there for the
           seconds the shutdown takes (#1712). -->
      <div class="pj">
        <div class="pr" class:on={project.shown === 'lead'}>
          <span class="dot {leadDot}"></span>
          <span class="nm"><a href={hrefForSlot(project.row.slot)}>{project.name}</a></span>
          <span class="org">{project.org}</span>
          {#if project.asleep}
            <span class="age">{project.age}</span>
          {:else}
            <CloseChip name={project.name} onclose={() => close(project.row.slot)} />
          {/if}
        </div>
        {#if project.why !== null}
          <div class="why" class:bad={project.why.bad}>{project.why.line}</div>
        {/if}
        {#each project.workers as worker (worker.slot.label)}
          {@const failed = failedLine(worker)}
          <div class="wk" class:on={project.shown === worker.slot.label}>
            <!-- No closing arm here: a closed worker folds into the sleeper
                 fold on the grouping, so this each never draws one. -->
            <span class="dot {railMark(worker.state)}"></span>
            <span class="nm"><a href={hrefForSlot(worker.slot)}>{worker.slot.label}</a></span>
            <CloseChip name={worker.slot.label} onclose={() => close(worker.slot)} />
          </div>
          <!-- The failure draws under the row that failed, dim as the
               terminal's sub-row draws it: the row's own failed dot carries
               the colour, and on the project's line the diagnostic would
               read as the lead's. -->
          {#if failed !== null}
            <div class="why">{failed}</div>
          {/if}
        {/each}
        <!-- The seats this project has asleep, behind one row that counts
             them: they are rows a reader is not working in, and the count is
             what keeps the fold from reading as a project with no workers. -->
        {#if project.sleeping.length > 0}
          <SleeperFold sleeping={project.sleeping} shown={project.shown} closing={closingSeat} />
        {/if}
      </div>
    {/snippet}

    {#each groups as group (group.heading)}
      {#if group.hidden === null}
        <div class={group.klass}>{group.heading}</div>
        {#each group.projects as project (project.org + '/' + project.name)}
          {@render projectBlock(project)}
        {/each}
      {:else}
        <GroupFold heading={group.heading} count={group.hidden} holds={group.holds}>
          {#each group.projects as project (project.org + '/' + project.name)}
            {@render projectBlock(project)}
          {/each}
        </GroupFold>
      {/if}
    {/each}
  </div>

  <!-- The rail's own footer: below the list, so the projects scroll behind it
       rather than with it. Its class is its own - `.foot` is the composer's
       key-hint line, and a second `.foot` would take that rule's mono and its
       size with it. -->
  <div class="rfoot">
    {#if foot.account !== null}
      <div class="who">
        <span class="led {foot.account.tone}"></span>
        <span class="an">{foot.account.name}</span>
      </div>
    {/if}
    {#if foot.figures.length > 0}
      <div class="frs">
        {#each foot.figures as figure (figure.label)}
          <div class="fr">
            <span class="k">{figure.label}</span>
            <span class="v" class:none={figure.dim}>{figure.value}</span>
          </div>
        {/each}
      </div>
    {/if}
    {#if foot.windows.length > 0}
      <div class="frs">
        {#each foot.windows as window (window.label)}
          <div class="bar">
            <span class="lb">{window.label}</span>
            <span class="tk"><span class="fl" style={`width:${window.percent}%`}></span></span>
            <span class="pc">{window.text}</span>
            {#if window.reset !== ''}<span class="eta">{window.reset}</span>{/if}
          </div>
        {/each}
      </div>
    {/if}
    <div class="vers">
      <div class="v">forge v{foot.versions.forge}</div>
      <div class="v">socket v{foot.versions.socket}</div>
      {#if foot.versions.claude !== null}
        <div class="v">
          <!-- The space between the version and the arrow is part of the row,
               not something the two nodes happen to be laid out with: written
               as a space at all it survives, and omitted it is the run-together
               pair the row drew before. -->
          claude v{foot.versions.claude}
          {#if foot.versions.update !== null}<span class="up"
              >{'\u{2192}'} v{foot.versions.update}</span
            >{/if}
        </div>
      {/if}
    </div>
  </div>
</aside>

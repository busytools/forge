<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { hrefForSlot } from '../routes';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import CloseChip from './CloseChip.svelte';
  import { fleetCount, railFooter, railGroups, railMark } from './view';

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
    onclose,
  }: {
    home: HomeWire;
    /** The seat the page is showing, which its own project's row marks. */
    current: SessionSlot;
    now: number;
    /** Brings the header's handle back, which the collapsed rail has covered. */
    onclose: () => void;
  } = $props();

  const groups = $derived(railGroups(home, current, now));
  /**
   * The account and the two builds, which sit under the list rather than in it:
   * the projects scroll behind them.
   */
  const foot = $derived(railFooter(home, current));
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
    {#each groups as group (group.heading)}
      <div class={group.klass}>{group.heading}</div>
      {#each group.projects as project (project.org + '/' + project.name)}
        <div class="pj" class:cur={project.current}>
          <div class="pr">
            <span class="dot {railMark(project.row.state)}"></span>
            <span class="nm"><a href={hrefForSlot(project.row.slot)}>{project.name}</a></span>
            <span class="org">{project.org}</span>
            {#if project.asleep}
              <span class="age">{project.age}</span>
            {:else}
              <CloseChip />
            {/if}
          </div>
          {#if project.why !== null}
            <div class="why" class:bad={project.why.bad}>{project.why.line}</div>
          {/if}
          {#each project.workers as worker (worker.slot.label)}
            <div class="wk">
              <span class="dot {railMark(worker.state)}"></span>
              <span class="nm"><a href={hrefForSlot(worker.slot)}>{worker.slot.label}</a></span>
              <CloseChip />
            </div>
          {/each}
        </div>
      {/each}
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
      {#if foot.versions.claude !== null}
        <div class="v">
          claude v{foot.versions.claude}{#if foot.versions.update !== null}<span class="up"
              >{'\u{2191}'} v{foot.versions.update}</span
            >{/if}
        </div>
      {/if}
    </div>
  </div>
</aside>

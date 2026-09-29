<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import Card from '../components/Card.svelte';
  import Row from '../components/Row.svelte';
  import type { HomeWire } from '../wire/home';
  import { countsOf, homeView } from './view';

  /**
   * The home: every project, its agents, their states, and what needs you.
   *
   * `wire` is required and has no default, because the app's only input is
   * the server URL: a page that fell back to bundled data when a server was
   * absent is the failure the standard names.
   */
  let {
    wire,
    address = '',
    mark = null,
  }: { wire: HomeWire; address?: string; mark?: string | null } = $props();

  const view = $derived(homeView(wire, address));
  // One clock for the page: every row's `when` reads against the same now,
  // so two rows a second apart cannot draw the same age differently.
  const now = Date.now();
</script>

<!-- A landmark, so every part of the page sits inside one. The sheet's
     `.wrap` rule is a class, so this changes nothing it draws. -->
<main class="wrap">
  <header class="top">
    <div class="brand">
      <Brand name={mark} />
      <span class="word">forge</span>
    </div>
    <div class="versions">
      <!-- The forge build serving the socket, not this app's own version:
           the header states which forge is running. -->
      <b>v{view.header.version}</b>
      {#if view.header.installed}{' \u{b7} '}claude {view.header.installed}{/if}
      {#if view.header.update}{' \u{b7} '}<span class="upd"
          >{'\u{2191}'} v{view.header.update} available</span
        >{/if}
    </div>
    <div class="totals">
      <!--
        The task count is not here: no total crosses the wire, and summing
        the rows a client happens to be showing would count a fleet it cannot
        see. The per-row task is drawn where it belongs, on the row.
      -->
      <span class="n">{view.header.liveAgents}</span> agents {'\u{b7}'}
      <span class="n">{view.header.projects}</span> projects
    </div>
  </header>

  <section class="band">
    {#each view.band as card (card.title)}
      <Card {...card} />
    {/each}
  </section>

  {#if view.orgs.length === 0}
    <div class="empty">
      <Brand name={mark} />
      <p class="t">No projects yet</p>
      <p class="d">
        Projects come from <code>forge.toml</code>. Add an
        <code>[[orgs.projects]]</code>
        entry pointing at a repository, and it appears here.
      </p>
    </div>
  {:else}
    {#each view.orgs as org (org.name)}
      <section class="org">
        <h2>
          {org.name}<span class="rule"></span>
          <span class="counts">{countsOf(org)}</span>
        </h2>
        <ul class="list">
          <!-- Org-scoped: two orgs may declare a project of the same name, and
               a key that collided would leave one of them unrendered. -->
          {#each org.projects as project (`${project.lead.slot.org}/${project.lead.slot.project}`)}
            <li class="node">
              <Row row={project.lead} {now} refused={project.refused} />
              {#if project.workers.length > 0}
                <ul class="children">
                  {#each project.workers as worker (worker.slot.label)}
                    <li class="node"><Row row={worker} {now} /></li>
                  {/each}
                </ul>
              {/if}
            </li>
          {/each}
        </ul>
      </section>
    {/each}
  {/if}
</main>

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

<div class="wrap">
  <header class="top">
    <div class="brand">
      <Brand name={mark} />
      <span class="word">forge</span>
    </div>
    <div class="versions">
      <!--
        forge's own version is not here: nothing on the wire carries it, and
        the server's home draws it from its own build. The claude version and
        the update notice both cross in `cli_version`.
      -->
      {#if view.header.installed}claude {view.header.installed}{/if}
      {#if view.header.update}{' \u{b7} '}<span class="upd"
          >{'\u{2191}'} v{view.header.update} available</span
        >{/if}
    </div>
    <div class="totals">
      <!--
        The task count is not here either, for the same reason: the snapshot
        carries no tasks, so a number would be invented rather than read.
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
</div>

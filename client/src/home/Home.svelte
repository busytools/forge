<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import Card from '../components/Card.svelte';
  import Row from '../components/Row.svelte';
  import { PROTOCOL_VERSION } from '../protocol';
  import { install, restart, updateState } from '../update/state';
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
  //
  // Re-read with every snapshot AND on a tick. A snapshot alone is not
  // enough: age is the one cell that is a function of time rather than of
  // data, and a re-read needs an update the fleet may never send, so a page
  // opened at nine on a quiet forge would still say "3h" at three.
  let now = $state(Date.now());
  $effect(() => {
    if (!wire) return;
    now = Date.now();
    const tick = setInterval(() => {
      now = Date.now();
    }, 30_000);
    return () => clearInterval(tick);
  });
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
      <!-- What this app speaks on the socket: the client and a server that
           disagrees on it refuse each other, so it reads beside the builds -
           with the same `v` the rail's own version lines spell, one spelling
           for the one number both corners show. -->
      {' \u{b7} '}socket v{PROTOCOL_VERSION}
      {#if view.header.installed}{' \u{b7} '}claude {view.header.installed}{/if}
      {#if view.header.update}{' \u{b7} '}<span class="upd"
          >{'\u{2191}'} v{view.header.update} available</span
        >{/if}
      <!-- This app's own update, beside the CLI notice it shares the idiom
           with: the client half is the one this window can act on, so it is
           the half that is a control. -->
      {#if $updateState.stage === 'available'}
        {' \u{b7} '}<button class="upd" onclick={install}
          >client {'\u{2191}'} v{$updateState.version} available</button
        >
      {:else if $updateState.stage === 'installing'}
        {' \u{b7} '}<span class="upd">client {'\u{2191}'} v{$updateState.version} updating...</span>
      {:else if $updateState.stage === 'restart'}
        {' \u{b7} '}<button class="upd" onclick={restart}
          >client v{$updateState.version} ready - restart to finish</button
        >
      {:else if $updateState.stage === 'failed'}
        {' \u{b7} '}<button class="upd" onclick={install}
          >client v{$updateState.version} - update failed, retry</button
        >
      {/if}
    </div>
    <div class="totals">
      <!-- `.n` on the first two only: the sheet weights the fleet's own
           counts bright and the project total quiet, which is the server's
           markup too. -->
      <span class="n">{view.header.liveAgents}</span> agents {'\u{b7}'}
      <span class="n">{view.header.tasks}</span> tasks {'\u{b7}'}
      {view.header.projects} projects
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

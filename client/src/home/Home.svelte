<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import Card from '../components/Card.svelte';
  import Mark from '../components/Mark.svelte';
  import { CLIENT_VERSION, PROTOCOL_VERSION } from '../protocol';
  import { install, restart, updateState } from '../update/state';
  import type { HomeWire } from '../wire/home';
  import { fleetRows, homeView, markOf } from './view';

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
  // The fleet: one row per project, nothing mixed. The counts and the
  // misses are the server's own; the mark is the strongest of the
  // project's seat marks, in the same vocabulary a seat row draws.
  const fleet = $derived(fleetRows(wire));
</script>

<!-- A landmark, so every part of the page sits inside one. The sheet's
     `.wrap` rule is a class, so this changes nothing it draws. -->
<main class="wrap">
  {#if wire.fatal_error !== null}
    <div class="stopped">forge stopped: {wire.fatal_error}</div>
  {/if}
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
      {:else if $updateState.stage === 'install'}
        {' \u{b7} '}<button class="upd" onclick={install}
          >client v{$updateState.version} ready - install it</button
        >
      {:else if $updateState.stage === 'failed'}
        {' \u{b7} '}<button class="upd" onclick={install} title={$updateState.detail}
          >client v{$updateState.version} - update failed, retry</button
        >
      {:else if $updateState.stage === 'web'}
        <!-- The browser build names itself and, when the image it was served
             with carries a manifest naming a newer release, that release -
             text rather than a control, because the next load carries it. -->
        {' \u{b7} '}client v{CLIENT_VERSION}
        {#if $updateState.latest}{' \u{b7} '}<span class="upd">latest v{$updateState.latest}</span
          >{/if}
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

  {#if fleet.length === 0}
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
    <ul class="fleet">
      <!-- Org-scoped in the key: two orgs may declare a project of the same
           name, and a key that collided would leave one of them unrendered. -->
      {#each fleet as row (`${row.org}/${row.name}`)}
        <li class="fleet-row">
          <!-- The `.row <state>` ancestor is what the sheet keys every dot
               shape on, so the fleet reuses the whole mark vocabulary. -->
          <span class="row {markOf(row.state).class}"><Mark state={row.state} /></span>
          <span class="fleet-name">{row.name}</span>
          <span class="fleet-where">
            {#if row.gate}{row.gate}{:else}
              {#if row.place.branch}{row.place.branch}{/if}
              {#if row.place.branch && row.place.files}{' \u{b7} '}{/if}
              {#if row.place.files}<span class="files">{row.place.files}</span>{/if}
            {/if}
          </span>
          <span class="fleet-seats">
            {#each row.seats as seat (seat.label)}
              <span class="fleet-seat"
                ><span class="row {markOf(seat.state).class}"><Mark state={seat.state} /></span
                >{seat.label}</span
              >
            {/each}
          </span>
          <span class="fleet-counts">
            {row.live} of {row.slots ?? '?'} slots {'\u{b7}'} queue {row.queue}
            {#if row.onYou > 0}
              {'\u{b7}'} <span class="fleet-you">on you {row.onYou}</span>
            {/if}
          </span>
          <span class="fleet-misses">
            {#each row.misses as miss (miss.label)}
              <span class="fleet-miss {miss.kind}">{miss.label}</span>
            {/each}
          </span>
          <a class="fleet-open" href={row.href}>open board {'\u{2192}'}</a>
        </li>
      {/each}
    </ul>
  {/if}
</main>

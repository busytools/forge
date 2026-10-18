<script lang="ts">
  import Brand from '../components/Brand.svelte';
  import Card from '../components/Card.svelte';
  import GroupFold from '../components/GroupFold.svelte';
  import Mark from '../components/Mark.svelte';
  import { watchExtensions, type ExtensionsPage } from '../extensions/live';
  import { CLIENT_VERSION, PROTOCOL_VERSION } from '../protocol';
  import type { Connection } from '../socket';
  import { install, restart, updateState } from '../update/state';
  import type { HomeWire } from '../wire/home';
  import { ageOf, extensionsCard, fleetGroups, homeView, markOf, type FleetViewRow } from './view';

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
    connection = null,
  }: {
    wire: HomeWire;
    address?: string;
    mark?: string | null;
    /** The socket, for the extensions card's own read. Optional, because a
     * rendered-without-a-connection home draws that card's reading state. */
    connection?: Connection | null;
  } = $props();

  const view = $derived(homeView(wire, address));
  /**
   * The extensions read, for the band's fifth card: the counts a reader
   * wants there (installed, behind its marketplace) are in the `extensions`
   * subject and nowhere on the home's own wire. The subscription is the
   * card's and goes with the page.
   */
  let extensions = $state<ExtensionsPage>({ read: null, refused: null });
  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchExtensions(open).subscribe((value) => {
      extensions = value;
    });
  });
  const band = $derived([
    ...view.band,
    extensionsCard(extensions.read, extensions.refused !== null),
  ]);
  // The fleet: one row per project, grouped by the same three states the
  // rail groups by - needs you, working, asleep - so a project reads in the
  // same section on either surface. The counts and the misses are the
  // server's own; the mark is the strongest of the project's seat marks, in
  // the same vocabulary a seat row draws.
  const fleet = $derived(fleetGroups(view.fleet));
  // One clock for the page: every row's age reads against the same now,
  // so two rows a second apart cannot draw the same age differently.
  //
  // The tick is the clock's only writer. Age is a function of time rather
  // than of data, so a quiet forge needs it - a page opened at nine would
  // otherwise still say "3h" at three - and a snapshot must not refresh it:
  // reading `wire` in the effect below re-arms the timer on every frame,
  // and a fleet writing faster than the tick would then freeze the clock
  // instead of moving it.
  /**
   * The row's tooltip: the name, the tree read and the age, because the
   * stretched link makes the whole row one hover target and a title on a
   * cell underneath it is never read.
   */
  function rowTitle(row: FleetViewRow): string {
    const where =
      row.gate ??
      [row.place.branch, row.place.files].filter((part) => part !== null).join(' \u{b7} ');
    return where === ''
      ? `${row.name} - when the project last moved`
      : `${row.name} - ${where} - when the project last moved`;
  }
  let now = $state(Date.now());
  /** The ready fact as a value, so the timer below does not restart on every update that rewrites `wire`. */
  const ready = $derived(wire !== null);
  $effect(() => {
    if (!ready) return;
    const tick = setInterval(() => {
      now = Date.now();
    }, 30_000);
    return () => clearInterval(tick);
  });
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
      <!-- The three builds this window is made of, each named: the server
           serving the socket, this client, and the CLI both drive. Each
           update notice sits with the build it belongs to, so a version
           offered there cannot be read as another build's. -->
      <span class="build"><span class="k">server</span> <b>v{view.header.version}</b></span>
      {' \u{b7} '}
      <span class="build"><span class="k">client</span> <b>v{CLIENT_VERSION}</b></span>
      <!-- This app's own update is the half this window can act on, so it is
           the one drawn as a control. -->
      {#if $updateState.stage === 'available'}
        <button
          class="upd"
          onclick={install}
          aria-label={`client \u{2191} v${$updateState.version} available`}
          >{'\u{2191}'} v{$updateState.version} available</button
        >
      {:else if $updateState.stage === 'installing'}
        <span class="upd">{'\u{2191}'} v{$updateState.version} updating...</span>
      {:else if $updateState.stage === 'restart'}
        <button
          class="upd"
          onclick={restart}
          aria-label={`client v${$updateState.version} ready - restart to finish`}
          >v{$updateState.version} ready - restart to finish</button
        >
      {:else if $updateState.stage === 'install'}
        <button
          class="upd"
          onclick={install}
          aria-label={`client v${$updateState.version} ready - install it`}
          >v{$updateState.version} ready - install it</button
        >
      {:else if $updateState.stage === 'failed'}
        <button
          class="upd"
          onclick={install}
          title={$updateState.detail}
          aria-label={`client v${$updateState.version} - update failed, retry`}
          >v{$updateState.version} - update failed, retry</button
        >
      {:else if $updateState.stage === 'web' && $updateState.latest}
        <!-- The browser build, when the image it was served with carries a
             manifest naming a newer release: text rather than a control,
             because the next load carries it. -->
        <span class="upd">latest v{$updateState.latest}</span>
      {/if}
      <!-- What the client and the server speak to each other: a mismatch is
           refused, so the number reads beside the two builds it binds. -->
      {' \u{b7} '}
      <span class="build"><span class="k">socket</span> <b>v{PROTOCOL_VERSION}</b></span>
      {#if view.header.installed}
        {' \u{b7} '}
        <span class="build"><span class="k">claude</span> <b>v{view.header.installed}</b></span>
        {#if view.header.update}
          <span class="upd">{'\u{2191}'} v{view.header.update} available</span>
        {/if}
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

  <!-- The core's held service status, above the band and only when there is
       one. **The snapshot is what carries it to this page**: the probe fires
       once, before a client attaches, and the live announcement is a chat
       row - so this is how a reader who opened the page after the report was
       raised learns of it. -->
  {#if view.upstream !== null}
    <div class="upstream" class:bad={view.upstream.severity === 'error'}>
      <span class="k">claude status</span>
      <span>{view.upstream.message}</span>
    </div>
  {/if}

  <section class="band">
    {#each band as card (card.title)}
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
    {#snippet fleetRow(row: FleetViewRow)}
      <!-- The age's tooltip lives on the row itself: the stretched link
           covers the age cell, so a title on it never shows. -->
      <li class="fleet-row" title={rowTitle(row)}>
        <!-- The `.row <state>` ancestor is what the sheet keys every dot
             shape on, so the fleet reuses the whole mark vocabulary. -->
        <span class="row {markOf(row.state).class}"><Mark state={row.state} /></span>
        <!-- The row's own way in: this link stretches over the whole row,
             so anywhere that is not another control opens the lead's
             chat. -->
        <a class="fleet-name fleet-go" href={row.leadHref}>{row.name}</a>
        <span class="fleet-where">
          {#if row.gate}{row.gate}{:else}
            {#if row.place.branch}{row.place.branch}{/if}
            {#if row.place.branch && row.place.files}{' \u{b7} '}{/if}
            {#if row.place.files}<span class="files">{row.place.files}</span>{/if}
          {/if}
        </span>
        <span class="fleet-when">{ageOf(row, now)}</span>
        <span class="fleet-seats">
          {#each row.seats as seat (seat.label)}
            <a class="fleet-seat" href={seat.href}
              ><span class="row {markOf(seat.state).class}"><Mark state={seat.state} /></span
              >{seat.label}</a
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
    {/snippet}
    <!-- The three sections are the rail's: the same rank, the same words.
         The asleep half starts open here, with its count and a fold for a
         reader who wants the quiet projects out of the way. -->
    {#each fleet as group (group.heading)}
      {#if group.heading === 'asleep'}
        <GroupFold heading={group.heading} count={group.count} mark={group.mark} startOpen={true}>
          <ul class="fleet">
            {#each group.rows as row (`${row.org}/${row.name}`)}
              {@render fleetRow(row)}
            {/each}
          </ul>
        </GroupFold>
      {:else}
        <div class={group.klass}>
          {group.heading} <span class="n">{group.count}</span>
        </div>
        <ul class="fleet">
          {#each group.rows as row (`${row.org}/${row.name}`)}
            {@render fleetRow(row)}
          {/each}
        </ul>
      {/if}
    {/each}
  {/if}
</main>

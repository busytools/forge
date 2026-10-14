<script lang="ts">
  /**
   * The Extensions page's drawing: the rail of kinds, the pane's rows, and
   * the plugin fold.
   *
   * The shape is the mock Ved settled on 2026-10-10 (set 2): kinds down the
   * left with their counts, one pane at a time, a plugin row that folds its
   * components out (one open at a time), and three marks in the whole
   * vocabulary - a triangle for an update, a spinner for work in flight, an
   * X for a failure. Everything rare is a chip in words.
   */
  import type { ProjectView } from '../wire/home';
  import Brand from '../components/Brand.svelte';
  import Chevron from '../components/Chevron.svelte';
  import InstallDialog from './InstallDialog.svelte';
  import type { ExtensionsRead } from '../wire/extensions';
  import {
    availableRows,
    band,
    kindRows,
    marketplaceDetail,
    marketplaceRows,
    pluginMark,
    pluginRows,
    railCounts,
    stalePlugins,
    type ComponentKind,
    type PaneId,
  } from './view';

  let {
    wire,
    refusal,
    failure = null,
    settled = 0,
    projects,
    mark = null,
    update,
    updateall,
    check,
    install,
    repair,
    addmarketplace,
  }: {
    wire: ExtensionsRead;
    /** A refused dispatch, in the core's own words. */
    refusal: string | null;
    /** A failed action or refresh, in the core's own words, from the live
     * stream: a failed update, install, add or repair reaches the page this
     * way, and a press with nothing said back reads as a press that did
     * nothing. */
    failure?: string | null;
    /** Bumped by the page on every plugin frame, so a press's own feedback
     * clears on the frames that answer it rather than on some later read. */
    settled?: number;
    projects: ProjectView[];
    mark?: string | null;
    update: (pluginId: string, scope: string) => void;
    updateall: () => void;
    check: () => void;
    install: (pluginId: string, scope: string, project: string | null) => void;
    repair: (name: string) => void;
    addmarketplace: (source: string) => void;
  } = $props();

  let pane = $state<PaneId>('installed');
  let query = $state('');
  /** The one open plugin's id; opening another closes this one. */
  let openId = $state<string | null>(null);
  /** The plugin an install is being scoped for, or `null`. */
  let installFor = $state<{ id: string; name: string } | null>(null);
  /** Where the keyboard was before the install dialog took it. */
  let opener: HTMLElement | null = null;
  /** A marketplace being added: whether the input is open, and its draft. */
  let adding = $state(false);
  let draft = $state('');
  /** The last update-all press's own key, cleared when the read moves. */
  let pressedAll = $state(false);
  /** The plugin a single update is riding on, so its row says so until the
   * frames land - the CLI call is seconds long and a silent chip reads as a
   * press that did nothing. */
  let pressedId = $state<string | null>(null);
  let checked = $state(false);

  const counts = $derived(railCounts(wire));
  const rows = $derived(pluginRows(wire));
  const stale = $derived(stalePlugins(wire));
  const updateBand = $derived(band(wire));
  const available = $derived(availableRows(wire));
  const marketplaces = $derived(marketplaceRows(wire));

  const RAIL: { id: PaneId; label: string }[] = [
    { id: 'installed', label: 'Installed' },
    { id: 'skills', label: 'Skills' },
    { id: 'agents', label: 'Agents' },
    { id: 'commands', label: 'Commands' },
    { id: 'hooks', label: 'Hooks' },
    { id: 'lsp', label: 'LSP' },
  ];

  function railCount(id: PaneId): number {
    switch (id) {
      case 'installed':
        return counts.installed;
      case 'skills':
        return counts.skills;
      case 'agents':
        return counts.agents;
      case 'commands':
        return counts.commands;
      case 'hooks':
        return counts.hooks;
      case 'lsp':
        return counts.lsp;
      case 'marketplaces':
        return counts.marketplaces;
    }
  }

  /** The active kind pane, when the rail is on one. */
  const kind = $derived<ComponentKind | null>(
    pane === 'skills' ||
      pane === 'agents' ||
      pane === 'commands' ||
      pane === 'hooks' ||
      pane === 'lsp'
      ? pane
      : null,
  );

  const kindPaneRows = $derived(kind === null ? [] : kindRows(wire, kind));

  function matches(text: string): boolean {
    const q = query.trim().toLowerCase();
    return q === '' || text.toLowerCase().includes(q);
  }

  const shownPlugins = $derived(rows.filter((row) => matches(`${row.id} ${row.detail}`)));
  const shownKind = $derived(kindPaneRows.filter((row) => matches(`${row.name} ${row.source}`)));
  const shownAvailable = $derived(available.filter((row) => matches(`${row.id} ${row.detail}`)));
  const shownMarketplaces = $derived(
    marketplaces.filter((row) => matches(`${row.name} ${row.source} ${row.repo ?? ''}`)),
  );

  // A press's own feedback clears on the FRAMES that answer it, not on some
  // later read: a single update moves no read on its own, so clearing on
  // the read would leave its row saying "updating..." for good. `settled`
  // ticks for every plugin frame, so anything the press started is over by
  // the time it moves.
  $effect(() => {
    void settled;
    void wire;
    pressedAll = false;
    pressedId = null;
    checked = false;
  });

  /** The draft of an add whose outcome has not landed: kept so a rejected
   * source is reopened with its words rather than silently discarded; the
   * frame count it was submitted at, so only a frame that lands AFTER it
   * counts as its outcome. */
  let pendingSource = $state<string | null>(null);
  let pendingFrom = $state(0);

  // The reopen rides the add's OWN outcome, which is a plugin frame (`settled`)
  // or the refusal of its dispatch - a refusal lands with no frame at all; a
  // SUCCESSFUL add lands with no failure at all. A source left pending past
  // either would ride some LATER, unrelated frame: reopening the input under a
  // line about something else, or being dropped silently. The frame must be
  // one that landed since the submission: without that, the effect's own
  // dependency on `pendingSource` would clear the draft the moment it is set.
  $effect(() => {
    const tick = settled;
    if (pendingSource === null) return;
    if (tick === pendingFrom && refusal === null) return;
    if (refusal === null && failure === null) {
      pendingSource = null;
      return;
    }
    adding = true;
    draft = pendingSource;
    pendingSource = null;
  });

  // The install dialog is an overlay, so it takes a history entry: the
  // hardware Back closes what it opened, alongside Esc and the backdrop.
  function openInstall(target: { id: string; name: string }, from: EventTarget | null): void {
    opener = from instanceof HTMLElement ? from : null;
    installFor = target;
    history.pushState(
      { ...(history.state as object | null), forgeOverlay: 'install' },
      '',
      location.href,
    );
  }

  function closeInstall(): void {
    installFor = null;
    const back = opener;
    opener = null;
    if (back !== null && back.isConnected) back.focus();
    if ((history.state as { forgeOverlay?: string } | null)?.forgeOverlay === 'install') {
      history.back();
    }
  }

  $effect(() => {
    const restore = () => {
      if (installFor !== null) closeInstall();
    };
    addEventListener('popstate', restore);
    return () => removeEventListener('popstate', restore);
  });

  function choose(next: PaneId): void {
    pane = next;
    query = '';
    openId = null;
  }

  function toggleFold(id: string): void {
    openId = openId === id ? null : id;
  }

  function submitAdd(): void {
    const source = draft.trim();
    if (source === '') return;
    addmarketplace(source);
    // The draft is kept until the outcome lands: a rejected source reopens
    // the input with its words rather than vanishing (see the effect above).
    // The frame it is measured from is this submission's own.
    pendingFrom = settled;
    pendingSource = source;
    adding = false;
    draft = '';
  }
</script>

<header class="top pagetop">
  <span class="brand"><Brand name={mark} /><span class="word">forge</span></span>
  <span class="title"
    ><span class="t">extensions</span><span class="s">everything this forge can run</span></span
  >
  <a class="back" href="/">&larr; home</a>
</header>

{#if refusal !== null || failure !== null}
  <p class="ext-said" role="alert">{refusal ?? failure}</p>
{/if}

<div class="ext-legend">
  <span><span class="dot warn"></span>an update is available</span>
  <span><span class="ring"></span>work in flight</span>
  <span><span class="dot failed"></span>failed - the reason is on the row</span>
  <span>no mark: installed and current</span>
</div>

{#if wire.snapshot === null}
  <!-- An inventory that cannot be read is its own whole state: the error
       alone, not the error above a healthy-looking empty page. -->
  <p class="pending">
    This forge could not read the plugin inventory: no CLAUDE_CONFIG_DIR or HOME resolves.
  </p>
{:else}
  <div class="ext-split">
    <div class="ext-rail" role="tablist" aria-label="Extension kinds" aria-orientation="vertical">
      {#each RAIL as item (item.id)}
        <button
          type="button"
          role="tab"
          id={'ext-tab-' + item.id}
          aria-controls={'ext-pane-' + item.id}
          class="ext-rail-item"
          class:on={pane === item.id}
          aria-selected={pane === item.id}
          onclick={() => choose(item.id)}
        >
          <span>{item.label}</span>
          {#if item.id === 'installed' && stale.length > 0}
            <span class="dot warn"></span>
          {/if}
          <span class="n">{railCount(item.id)}</span>
        </button>
      {/each}
      <div class="ext-rule"></div>
      <button
        type="button"
        role="tab"
        id="ext-tab-marketplaces"
        aria-controls="ext-pane-marketplaces"
        class="ext-rail-item"
        class:on={pane === 'marketplaces'}
        aria-selected={pane === 'marketplaces'}
        onclick={() => choose('marketplaces')}
      >
        <span>Marketplaces</span>
        <span class="n">{counts.marketplaces}</span>
      </button>
    </div>

    <div
      class="ext-pane"
      role="tabpanel"
      id={'ext-pane-' + pane}
      aria-labelledby={'ext-tab-' + pane}
    >
      {#if pane === 'marketplaces'}
        <div class="ext-pane-head">
          <span class="t">Marketplaces</span>
          <span class="s">{counts.marketplaces} configured</span>
          <span class="right">
            {#if adding}
              <input
                class="ext-add-in"
                data-editor="nowhere"
                placeholder="owner/repo or URL"
                bind:value={draft}
                onkeydown={(event) => {
                  if (event.key === 'Enter') submitAdd();
                  if (event.key === 'Escape') {
                    adding = false;
                    draft = '';
                  }
                }}
              />
              <button
                type="button"
                class="btn go"
                disabled={draft.trim() === ''}
                onclick={submitAdd}
              >
                add
              </button>
              <button
                type="button"
                class="btn ghost"
                onclick={() => {
                  adding = false;
                  draft = '';
                }}>cancel</button
              >
            {:else}
              <button type="button" class="btn add" onclick={() => (adding = true)}
                >add marketplace</button
              >
            {/if}
          </span>
        </div>
        <input
          class="find"
          placeholder="filter marketplaces..."
          data-editor="nowhere"
          bind:value={query}
        />
        {#each shownMarketplaces as row (row.name)}
          <div class="ext-mp">
            {#if row.problem !== null}<span class="dot warn" aria-hidden="true"></span>{/if}
            <span class="ext-nm">{row.name}</span>
            <span class="ext-src">{row.source}</span>
            <span class="ext-src">{marketplaceDetail(row)}</span>
            {#if row.problem !== null}
              <span class="ext-src ext-problem">{row.problem}</span>
            {/if}
            {#if row.problem !== null}
              <span class="side"
                ><button type="button" class="chip warn" onclick={() => repair(row.name)}
                  >repair</button
                ></span
              >
            {/if}
          </div>
        {/each}
        <p class="ext-note">Repair is the CLI's remove-and-re-add pair, then a re-scan.</p>
      {:else}
        <div class="ext-pane-head">
          <span class="t"
            >{pane === 'installed' ? 'Installed' : pane[0]?.toUpperCase() + pane.slice(1)}</span
          >
          <span class="s">
            {#if pane === 'installed'}
              {counts.installed} plugins · {counts.skills} skills · {counts.agents} agents · {counts.commands}
              commands
            {:else if kind !== null}
              {kindPaneRows.length} installed · every row names the plugin it comes from
            {/if}
          </span>
          <span class="right">
            <button
              type="button"
              class="btn ghost"
              disabled={checked}
              onclick={() => {
                checked = true;
                check();
              }}
            >
              {checked ? 'checking...' : 'check for updates'}
            </button>
          </span>
        </div>
        <input
          class="find"
          placeholder={pane === 'installed'
            ? 'filter plugins and their components...'
            : 'filter...'}
          data-editor="nowhere"
          bind:value={query}
        />

        {#if updateBand !== null}
          {#if updateBand.kind === 'stale'}
            <div class="ext-band in">
              <span class="dot warn"></span>
              <span class="t">{updateBand.line}</span>
              <span class="pairs">{updateBand.detail}</span>
              <span class="right">
                <button
                  type="button"
                  class="btn go"
                  disabled={pressedAll}
                  onclick={() => {
                    pressedAll = true;
                    updateall();
                  }}
                >
                  {pressedAll ? 'updating...' : 'update all'}
                </button>
              </span>
            </div>
          {:else if updateBand.kind === 'running'}
            <div class="ext-band in">
              <span class="ring"></span>
              <span class="t">{updateBand.line}</span>
              <span class="pairs">{updateBand.detail ?? ''}</span>
            </div>
          {:else}
            <div class="ext-band done">
              <span class="dot ok"></span>
              <span class="t">{updateBand.line}</span>
              <span class="pairs">{updateBand.detail ?? ''}</span>
            </div>
          {/if}
        {/if}

        {#if pane === 'installed'}
          {#each shownPlugins as row (row.id)}
            {@const mark = pluginMark(row)}
            <div class="ext-group">
              <div class="ext-row" class:open={openId === row.id} class:failed={mark === 'failed'}>
                <!-- The whole row is the fold toggle: a row that answered only
                   on its chevron would be furniture on a phone. The overlay
                   sits behind the row's own controls so they stay pressable. -->
                <button
                  type="button"
                  class="pick"
                  aria-expanded={openId === row.id}
                  aria-label="{openId === row.id ? 'close' : 'open'} {row.name}"
                  onclick={() => toggleFold(row.id)}
                ></button>
                <Chevron />
                {#if mark === 'update'}<span class="dot warn"></span>{/if}
                {#if mark === 'running'}<span class="ring"></span>{/if}
                {#if mark === 'failed'}<span class="dot failed"></span>{/if}
                <span class="ext-nm">{row.name}</span>
                <span class="ext-src">@{row.marketplace}</span>
                <span class="ext-src">
                  {row.version ?? 'no version'}{#if row.availableVersion !== null}<span class="to">
                      &rarr; {row.availableVersion}</span
                    >{/if}
                </span>
                <span class="ext-src">{row.detail}</span>
                <span class="side">
                  {#if row.auto}<span class="chip dim">auto-installed</span>{/if}
                  {#if !row.enabled}<span class="chip">disabled</span>{/if}
                  {#if row.runDetail !== null && row.runDetail.includes('restart required')}
                    <span class="chip warn">restart required</span>
                  {/if}
                  {#if row.runStatus === 'updating' || pressedId === row.id}
                    <span class="chip">updating...</span>
                  {:else if row.runStatus === 'failed'}
                    <!-- The reason is the row's own: the legend and the page
                       both say the failure's words ride the row. -->
                    <span class="chip bad">{row.name}: {row.runDetail ?? 'update failed'}</span>
                  {:else if row.availableVersion !== null}
                    <button
                      type="button"
                      class="chip warn"
                      onclick={() => {
                        pressedId = row.id;
                        update(row.id, row.scope);
                      }}>update</button
                    >
                  {/if}
                </span>
              </div>
              {#if openId === row.id}
                <div class="ext-subs">
                  {#if row.loadError !== null}
                    <div class="ext-subkind">
                      load failure <span class="c">{row.loadError}</span>
                    </div>
                  {:else if row.groups.length === 0}
                    <div class="ext-subkind">
                      no components <span class="c">the manifest declares none</span>
                    </div>
                  {:else}
                    {#each row.groups as group (group.kind)}
                      <div class="ext-subkind">
                        {group.label} <span class="c">{group.count}</span>
                      </div>
                      <div class="ext-subwrap">
                        {#each group.rows as sub (sub.name)}
                          <span class="ext-sub"
                            ><span class="k">{sub.name}</span>{#if sub.note !== null}<span
                                class="w"
                              >
                                - {sub.note}</span
                              >{/if}</span
                          >
                        {/each}
                      </div>
                    {/each}
                  {/if}
                </div>
              {/if}
            </div>
          {/each}
          <p class="ext-note">
            {shownPlugins.length} of {rows.length} plugins shown · a row's components ride its own fold
          </p>
        {:else if kind !== null}
          {#each shownKind as row (`${row.sourceId}:${row.name}`)}
            <div class="ext-row flat">
              {#if row.stale}<span class="dot warn"></span>{/if}
              {#if row.runStatus === 'updating'}<span class="ring"></span>{/if}
              <span class="ext-nm">{row.name}</span>
              <span class="ext-src">{row.source}</span>
              {#if row.note !== null}<span class="ext-src">{row.note}</span>{/if}
              <span class="side">
                {#if row.runStatus === 'updating' || pressedId === row.sourceId}
                  <span class="chip">updating...</span>
                {:else if row.stale}
                  <button
                    type="button"
                    class="chip warn"
                    onclick={() => {
                      pressedId = row.sourceId;
                      update(row.sourceId, row.sourceScope);
                    }}>update</button
                  >
                {/if}
              </span>
            </div>
          {/each}
          <p class="ext-note">{shownKind.length} of {kindPaneRows.length} rows shown</p>
        {/if}

        {#if pane === 'installed' || kind !== null}
          <div class="ext-sect">
            From the marketplaces <span class="c">{available.length} not installed</span>
          </div>
          {#each shownAvailable as row (row.id)}
            <div class="ext-row flat avail">
              <span class="ext-nm">{row.name}</span>
              <span class="ext-src">@{row.marketplace}</span>
              <span class="ext-src">{row.version ?? ''}</span>
              <span class="ext-src">{row.detail}</span>
              <span class="side">
                <button
                  type="button"
                  class="chip blue"
                  onclick={(event) =>
                    openInstall({ id: row.id, name: row.name }, event.currentTarget)}
                  >install</button
                >
              </span>
            </div>
          {/each}
          {#if shownAvailable.length === 0}
            <p class="ext-note">every marketplace plugin is either installed or filtered out</p>
          {/if}
          <p class="ext-note">
            a skill is never installed or updated alone: the press acts on its plugin, and the
            Installed pane's fold is where a plugin's components live.
          </p>
        {/if}
      {/if}
    </div>
  </div>
{/if}

{#if installFor !== null}
  <InstallDialog
    plugin={installFor.name}
    {projects}
    oninstall={(scope: string, project: string | null) => {
      const target = installFor;
      closeInstall();
      if (target !== null) install(target.id, scope, project);
    }}
    oncancel={closeInstall}
  />
{/if}

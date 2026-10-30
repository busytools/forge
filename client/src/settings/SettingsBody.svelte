<script lang="ts">
  import { flushSync } from 'svelte';

  import Brand from '../components/Brand.svelte';
  import ClassMark from '../components/ClassMark.svelte';
  import { PROTOCOL_VERSION } from '../protocol';
  import type { SettingsDocument } from '../wire/settings';
  import ValueCell from './ValueCell.svelte';
  import { builtInFor, classesOf, groupsOf, leafOf, selectedArea } from './view';

  /**
   * The page's drawing, prop-only: the document the server pushed, the
   * area the sub-nav picked, and the build facts the header names.
   */
  let {
    document,
    mark,
    cliVersion,
    selected,
    onSelect,
  }: {
    document: SettingsDocument;
    mark: string | null;
    cliVersion: string | null;
    selected: string | null;
    onSelect: (id: string) => void;
  } = $props();

  const shown = $derived(selectedArea(document, selected));
  const area = $derived(shown.area);
  const groups = $derived(area === null ? [] : groupsOf(area));
  let heading: HTMLHeadingElement | undefined = $state();

  /** Open the first area and put the reader at the pane it opened. */
  function openFirst(id: string): void {
    onSelect(id);
    flushSync();
    heading?.focus();
  }
</script>

<header class="top pagetop">
  <span class="brand"><Brand name={mark} /><span class="word">forge</span></span>
  <span class="title">
    <span class="t">settings</span>
    <span class="s">the effective config, as loaded</span>
  </span>
  <span class="settings-meta">
    <span class="versions">
      <span class="build"><span class="k">server</span> <b>v{document.version}</b></span> &middot;
      <span class="build"><span class="k">socket</span> <b>v{PROTOCOL_VERSION}</b></span> &middot;
      <span class="build"
        ><span class="k">claude</span>
        <b>{cliVersion === null ? 'unknown' : `v${cliVersion}`}</b></span
      >
    </span>
    <span class="totals">
      <span class="n">{document.totals.keys}</span> keys &middot;
      <span class="n">{document.totals.areas}</span> areas &middot;
      <code>{document.source}</code>
    </span>
  </span>
  <span class="settings-mode">read-only</span>
  <a class="back" href="/">&larr; home</a>
</header>

<div class="settings-legend" aria-label="what the effect classes mean">
  <span class="settings-legend-item"
    ><ClassMark held="live" /> reads the change on its next use</span
  >
  <span class="settings-legend-item"
    ><ClassMark held="next_session" /> read when a session or connection opens</span
  >
  <span class="settings-legend-item"><ClassMark held="restart" /> built once at boot</span>
  <span class="settings-legend-item"><ClassMark held="not_read_yet" /> nothing reads this key</span>
</div>
<p class="settings-legend-note">
  an area shows every class its keys hold; a dashed ring means a key there is read by nothing.
</p>

<div class="settings-split">
  <nav class="settings-rail" aria-label="config areas">
    {#each document.areas as row (row.id)}
      <button
        type="button"
        class="settings-rail-item"
        class:on={area?.id === row.id}
        aria-pressed={area?.id === row.id}
        onclick={() => onSelect(row.id)}
      >
        <span class="settings-rail-name">{row.name}</span>
        <span class="settings-rail-class">
          {#each classesOf(row.keys) as held (held)}<ClassMark {held} icon />{/each}
        </span>
        <span class="settings-rail-count">{row.keys.length}</span>
      </button>
    {/each}
  </nav>

  <section class="settings-pane" aria-labelledby="settings-area-title">
    <div class="settings-pane-head">
      <h2 class="t" id="settings-area-title" tabindex="-1" bind:this={heading}>
        {area?.name ?? (shown.missing ? (selected ?? 'that area') : 'settings')}
      </h2>
      {#if area !== null}
        <span class="s">{area.note}</span>
        <span class="settings-keys">{area.keys.length} keys</span>
      {/if}
    </div>

    {#if shown.missing}
      <p class="settings-empty">
        That area is not in this config.
        {#each document.areas.slice(0, 1) as first (first.id)}
          <button
            type="button"
            class="settings-link"
            onclick={() => {
              openFirst(first.id);
            }}
          >
            Open {first.name}
          </button>
        {/each}
      </p>
    {:else if area === null}
      <p class="settings-empty">This config declares no areas.</p>
    {:else if area.keys.length === 0}
      <p class="settings-empty">This area has no keys.</p>
    {:else}
      {#each groups as group (group.label)}
        {#if group.label !== ''}
          <div class="settings-group">
            <span class="settings-group-name">{group.label}</span>
            <span class="settings-group-class"><ClassMark held={group.class} /></span>
          </div>
        {/if}
        <table class="settings-rows">
          <caption class="vh">{area.name}{group.label === '' ? '' : ` ${group.label}`} keys</caption
          >
          <colgroup><col class="k" /><col /><col class="e" /></colgroup>
          <thead class="vh">
            <tr><th scope="col">key</th><th scope="col">value</th><th scope="col">effect</th></tr>
          </thead>
          <tbody>
            {#each group.rows as key (key.path)}
              <tr>
                <th scope="row"><code title={key.path}>{leafOf(key.path)}</code></th>
                <td class="v">
                  <ValueCell value={key.value} builtIn={builtInFor(area.id, key.path)} />
                </td>
                <td class="e"><ClassMark held={key.class} label={key.classLabel} /></td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/each}
    {/if}
  </section>
</div>

<script lang="ts">
  import { flushSync } from 'svelte';

  import Brand from '../components/Brand.svelte';
  import ClassMark from '../components/ClassMark.svelte';
  import { PROTOCOL_VERSION } from '../protocol';
  import type { SettingsDocument, SettingsEditSpec, SettingsKey } from '../wire/settings';
  import EditCell from './EditCell.svelte';
  import ValueCell from './ValueCell.svelte';
  import { ENTRY_FIELDS, addableGroups, currentEntryValue, currentValue } from './editor';
  import type { Draft } from './editor';
  import { builtInFor, classesOf, groupsOf, selectedArea } from './view';

  /**
   * The page's drawing, prop-only: the document the server pushed, the area
   * the sub-nav picked, and the build facts the header names. In edit mode
   * every value is its control and the group headers carry the add and
   * remove doors.
   */
  let {
    document,
    mark,
    cliVersion,
    selected,
    onSelect,
    editing = false,
    draft,
    onToggleEditing,
    onValue,
    onEntryValue,
    onAdd,
    onRemoveRequest,
    onCancelAdd,
    onUndoRemove,
    restart = [],
    errorPath = null,
    onDismissRestart,
  }: {
    document: SettingsDocument;
    mark: string | null;
    cliVersion: string | null;
    selected: string | null;
    onSelect: (id: string) => void;
    editing?: boolean;
    draft: Draft;
    onToggleEditing: () => void;
    onValue: (key: SettingsKey, value: unknown) => void;
    onEntryValue: (path: string, field: string, value: unknown) => void;
    onAdd: (area: string, group: string, parent: string | null) => void;
    onRemoveRequest: (entry: {
      area: string;
      group: string;
      path: string;
      label: string;
      parent: string | null;
    }) => void;
    onCancelAdd: (path: string) => void;
    onUndoRemove: (path: string) => void;
    restart?: string[];
    /** The key an invalid save named, marked in its row. */
    errorPath?: string | null;
    onDismissRestart: () => void;
  } = $props();

  const shown = $derived(selectedArea(document, selected));
  const area = $derived(shown.area);
  const groups = $derived(area === null ? [] : groupsOf(area));
  let heading: HTMLHeadingElement | undefined = $state();
  const added = $derived(
    area === null ? [] : Object.values(draft.adds).filter((entry) => entry.area === area.id),
  );

  /** Open the first area and put the reader at the pane it opened. */
  function openFirst(id: string): void {
    onSelect(id);
    flushSync();
    heading?.focus();
  }

  function addedSpec(
    group: string,
    field: { kind: string; allowed?: string[]; required: boolean },
  ): SettingsEditSpec {
    return {
      kind: field.kind as SettingsEditSpec['kind'],
      allowed: field.allowed ?? [],
      required: field.required,
      group,
    };
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
  <button
    type="button"
    class="settings-mode"
    class:editing
    aria-pressed={editing}
    onclick={onToggleEditing}
  >
    {editing ? 'editing' : 'edit'}
  </button>
  <a class="back" href="/">&larr; home</a>
</header>

{#if restart.length > 0}
  <div class="settings-restart" role="status">
    <span class="t">saved. these need a server restart:</span>
    <span class="keys">{restart.join(' · ')}</span>
    <button type="button" class="settings-link" onclick={onDismissRestart}>dismiss</button>
  </div>
{/if}

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
        {#if editing}
          {#each addableGroups(area.id) as group (group)}
            <button type="button" class="settings-add" onclick={() => onAdd(area.id, group, null)}>
              + {group}
            </button>
          {/each}
        {/if}
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
    {:else if area.keys.length === 0 && added.length === 0}
      <p class="settings-empty">This area has no keys.</p>
    {:else}
      {#each groups as group (group.label)}
        {@const removed =
          editing && group.entryPath !== null ? draft.removes[group.entryPath] : undefined}
        {#if group.label !== ''}
          <div class="settings-group" class:removing={removed !== undefined}>
            <span class="settings-group-name">{group.label}</span>
            <span class="settings-group-class"><ClassMark held={group.class} /></span>
            {#if editing && group.entryGroup === 'org' && group.parent === null}
              <button
                type="button"
                class="settings-row-action"
                onclick={() => onAdd(area.id, 'project', group.label)}
              >
                + project
              </button>
            {/if}
            {#if removed !== undefined}
              <span class="settings-removed">removed on save</span>
              <button
                type="button"
                class="settings-row-action"
                onclick={() => onUndoRemove(removed.path)}
              >
                undo
              </button>
            {:else if editing && group.entryPath !== null && group.entryGroup !== null && group.entryGroup !== 'org'}
              <button
                type="button"
                class="settings-row-action danger"
                onclick={() =>
                  onRemoveRequest({
                    area: area.id,
                    group: group.entryGroup ?? '',
                    path: group.entryPath ?? '',
                    label: group.label,
                    parent: group.parent,
                  })}
              >
                remove
              </button>
            {/if}
          </div>
        {/if}
        {#if removed === undefined}
          <table class="settings-rows">
            <caption class="vh"
              >{area.name}{group.label === '' ? '' : ` ${group.label}`} keys</caption
            >
            <colgroup><col class="k" /><col /><col class="e" /></colgroup>
            <thead class="vh">
              <tr><th scope="col">key</th><th scope="col">value</th><th scope="col">effect</th></tr>
            </thead>
            <tbody>
              {#each group.rows as key (key.path)}
                <tr class:bad={errorPath === key.path}>
                  <th scope="row">
                    <code title={key.path}>{key.leaf}</code>
                    {#if editing && key.edit.required}<span class="settings-required">required</span
                      >{/if}
                    {#if errorPath === key.path}<span class="settings-error">error</span>{/if}
                  </th>
                  <td class="v">
                    {#if editing}
                      <EditCell
                        path={key.path}
                        spec={key.edit}
                        value={currentValue(draft, key)}
                        onchange={(next) => onValue(key, next)}
                      />
                    {:else}
                      <ValueCell value={key.value} builtIn={builtInFor(area.id, key.path)} />
                    {/if}
                  </td>
                  <td class="e"><ClassMark held={key.class} label={key.classLabel} /></td>
                </tr>
              {/each}
            </tbody>
          </table>
        {/if}
      {/each}
      {#each added as entry (entry.path)}
        <div class="settings-group new">
          <span class="settings-group-name">{entry.label}</span>
          <span class="settings-new">new {entry.group}</span>
          <button type="button" class="settings-row-action" onclick={() => onCancelAdd(entry.path)}>
            discard
          </button>
        </div>
        <table class="settings-rows">
          <caption class="vh">new {entry.group} {entry.label}</caption>
          <colgroup><col class="k" /><col /><col class="e" /></colgroup>
          <tbody>
            {#each ENTRY_FIELDS[entry.group] ?? [] as field (field.key)}
              <tr>
                <th scope="row">
                  <code>{field.key}</code>
                  {#if field.required}<span class="settings-required">required</span>{/if}
                </th>
                <td class="v">
                  <EditCell
                    path={`${entry.path}.${field.key}`}
                    spec={addedSpec(entry.group, field)}
                    value={currentEntryValue(draft, entry.path, field.key)}
                    onchange={(next) => onEntryValue(entry.path, field.key, next)}
                  />
                </td>
                <td class="e"><span class="settings-new">new</span></td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/each}
    {/if}
  </section>
</div>

<script lang="ts">
  import { flushSync } from 'svelte';

  import Brand from '../components/Brand.svelte';
  import ClassMark from '../components/ClassMark.svelte';
  import { PROTOCOL_VERSION } from '../protocol';
  import type { SettingsDocument, SettingsEditSpec, SettingsKey } from '../wire/settings';
  import InstanceCard from './InstanceCard.svelte';
  import SettingRow from './SettingRow.svelte';
  import { ENTRY_FIELDS, addableGroups, currentEntryValue, currentValue } from './editor';
  import type { Draft } from './editor';
  import { builtInFor, classesOf, groupsOf, selectedArea } from './view';

  /**
   * The page's drawing, prop-only: the document the server pushed, the area
   * the sub-nav picked, and the build facts the header names. Each instance
   * draws as a card and each key as a line inside it; in edit mode the line's
   * value is its control and the card heads carry the add and remove doors.
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
    saving = false,
    focusAdd = null,
    errorPath = null,
    onBack = () => {},
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
    /** A save is in flight: the surface is inert until its answer. */
    saving?: boolean;
    /** The added entry whose first field takes the keyboard. */
    focusAdd?: string | null;
    /** The key an invalid save named, marked in its row. */
    errorPath?: string | null;
    /** The back link, intercepted while the draft is dirty. */
    onBack?: (event: MouseEvent) => void;
  } = $props();

  const shown = $derived(selectedArea(document, selected));
  const area = $derived(shown.area);
  const groups = $derived(area === null ? [] : groupsOf(area));
  // A project's area holds its orgs' own rows too; the summary counts the
  // projects, not the orgs that carry them.
  const instances = $derived(
    groups.filter((group) =>
      area?.id === 'projects' ? group.entryGroup === 'project' : group.label !== '',
    ).length,
  );
  let heading: HTMLHeadingElement | undefined = $state();
  const added = $derived(
    area === null ? [] : Object.values(draft.adds).filter((entry) => entry.area === area.id),
  );

  // A new entry draws at the end of the pane; without this a tap near the top
  // looks like nothing happened. The first field takes the keyboard and the
  // entry is scrolled into view.
  $effect(() => {
    if (focusAdd === null) return;
    // `document` is the settings document here; the DOM is globalThis's.
    const cell = globalThis.document.getElementById(`settings-add-first-${focusAdd}`);
    if (cell === null) return;
    cell.querySelector<HTMLElement>('input, select, button')?.focus();
    // jsdom has no layout; the browser does.
    cell.scrollIntoView?.({ block: 'nearest' });
  });

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

  /** What an area's instances are called in its summary. */
  function instanceWord(id: string): string {
    const noun =
      id === 'accounts'
        ? 'account'
        : id === 'projects'
          ? 'project'
          : id === 'models'
            ? 'model'
            : id === 'server'
              ? 'table'
              : 'group';
    return instances === 1 ? noun : `${noun}s`;
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
  <a class="back" href="/" onclick={onBack}>&larr; home</a>
  <button
    type="button"
    class="settings-mode"
    class:editing
    aria-pressed={editing}
    disabled={saving}
    onclick={onToggleEditing}
  >
    {editing ? 'editing' : 'edit'}
  </button>
</header>

<div class="settings-split" inert={saving}>
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
    <header class="settings-pane-head">
      <h2 class="t" id="settings-area-title" tabindex="-1" bind:this={heading}>
        {area?.name ?? (shown.missing ? (selected ?? 'that area') : 'settings')}
      </h2>
      {#if area !== null}
        <span class="settings-pane-note">{area.note}</span>
        <span class="settings-pane-summary"
          >{area.keys.length} keys{instances === 0
            ? ''
            : ` / ${instances} ${instanceWord(area.id)}`}</span
        >
        {#if editing}
          {#each addableGroups(area.id) as group (group)}
            <button type="button" class="settings-add" onclick={() => onAdd(area.id, group, null)}>
              + {group}
            </button>
          {/each}
        {/if}
      {/if}
    </header>

    <div class="settings-pane-body">
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
        {#each groups as group, index (group.label)}
          {@const removed =
            editing && group.entryPath !== null ? draft.removes[group.entryPath] : undefined}
          <InstanceCard
            label={group.label}
            count={group.rows.length}
            headingId={group.label === '' ? null : `card-${area.id}-${index}`}
            removed={removed !== undefined}
          >
            {#snippet actions()}
              {#if removed !== undefined}
                <button
                  type="button"
                  class="settings-row-action"
                  onclick={() => onUndoRemove(removed.path)}
                >
                  undo
                </button>
              {:else if editing && group.entryGroup === 'org' && group.parent === null}
                <button
                  type="button"
                  class="settings-row-action"
                  onclick={() => onAdd(area.id, 'project', group.label)}
                >
                  + project
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
            {/snippet}
            {#each group.rows as key (key.path)}
              <SettingRow
                leaf={key.leaf}
                path={key.path}
                held={key.class}
                classLabel={key.classLabel}
                required={key.edit.required}
                bad={errorPath === key.path}
                dirty={key.path in draft.values}
                {editing}
                spec={key.edit}
                value={currentValue(draft, key)}
                shown={key.value}
                builtIn={builtInFor(area.id, key.path)}
                onchange={(next: unknown) => onValue(key, next)}
              />
            {/each}
          </InstanceCard>
        {/each}
        {#each added as entry (entry.path)}
          {@const fields = ENTRY_FIELDS[entry.group] ?? []}
          <InstanceCard label={entry.label} headingId={`card-add-${entry.path}`} fresh>
            {#snippet actions()}
              <span class="settings-new">new {entry.group}</span>
              <button
                type="button"
                class="settings-row-action"
                onclick={() => onCancelAdd(entry.path)}
              >
                discard
              </button>
            {/snippet}
            {#each fields as field, at (field.key)}
              <SettingRow
                leaf={field.key}
                path={`${entry.path}.${field.key}`}
                required={field.required}
                editing
                spec={addedSpec(entry.group, field)}
                value={currentEntryValue(draft, entry.path, field.key)}
                fieldId={at === 0 ? `settings-add-first-${entry.path}` : undefined}
                onchange={(next: unknown) => onEntryValue(entry.path, field.key, next)}
              />
            {/each}
          </InstanceCard>
        {/each}
      {/if}
    </div>
  </section>
</div>

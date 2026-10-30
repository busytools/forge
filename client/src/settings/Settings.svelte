<script lang="ts">
  import type { HomeRead } from '../home/live';
  import type { Connection } from '../socket';
  import type { SettingsDocument, SettingsEditRequest, SettingsKey } from '../wire/settings';
  import SettingsBody from './SettingsBody.svelte';
  import { settingsFrame } from './frame';
  import {
    addEntry,
    dirtyCount,
    editRequests,
    emptyDraft,
    removeEntry,
    restartKeys,
    setEntryValue,
    setValue,
    undoRemove,
  } from './editor';
  import type { Draft } from './editor';

  /**
   * The settings route: it draws the document the server pushed and, in edit
   * mode, batches its changes into one save. Success is the settings push the
   * reload produces; a save that writes nothing is answered with
   * `settings_refused`, and a push that never arrives is a timeout the bar
   * reports.
   */
  let { connection, home, mark }: { connection: Connection; home: HomeRead; mark: string | null } =
    $props();

  const frame = $derived($settingsFrame);
  const cliVersion = $derived(home.wire?.cli_version?.installed ?? null);

  type EntryTarget = {
    area: string;
    group: string;
    path: string;
    label: string;
    parent: string | null;
  };

  let selected = $state<string | null>(null);
  let editing = $state(false);
  let draft = $state<Draft>(emptyDraft());
  let saving = $state(false);
  let refusal = $state<{
    reason: 'stale' | 'invalid' | 'refused';
    message: string;
    keyPath: string | null;
  } | null>(null);
  let restart = $state<string[]>([]);
  let confirm = $state<EntryTarget | null>(null);

  /** The document the save was made against, and its requests, for the banner. */
  let savedAgainst = $state<{ document: SettingsDocument; requests: SettingsEditRequest[] } | null>(
    null,
  );
  /** The fingerprint the pending save was made against. */
  let savedFrom = $state<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | null = null;

  const dirty = $derived(dirtyCount(draft));

  function clearTimer(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  function discard(): void {
    draft = emptyDraft();
    refusal = null;
  }

  // A push after a save is its success: the new fingerprint is what says the
  // reload landed, and the banner lists the restart-class keys it carried.
  $effect(() => {
    const held = $settingsFrame;
    if (held.status !== 'ready' || !saving || savedFrom === null) return;
    if (held.document.fingerprint === savedFrom) return;
    const against = savedAgainst;
    if (against !== null) {
      restart = restartKeys(against.document, held.document, against.requests);
    }
    draft = emptyDraft();
    saving = false;
    refusal = null;
    savedFrom = null;
    savedAgainst = null;
    clearTimer();
  });

  // A refusal is the page's own line; the save bar draws it.
  $effect(() => {
    const stop = connection.onMessage((message) => {
      if (message.kind !== 'settings_refused') return;
      saving = false;
      clearTimer();
      refusal = {
        reason: message.reason,
        message: message.message,
        keyPath: message.key_path ?? null,
      };
    });
    return () => {
      stop();
      clearTimer();
    };
  });

  function save(): void {
    if (frame.status !== 'ready' || dirty === 0 || saving) return;
    const document = frame.document;
    const requests = editRequests(draft, document);
    if (requests.length === 0) return;
    const wire = requests.map((request) => ({
      area: request.area,
      key_path: request.keyPath,
      pattern: request.pattern,
      op: request.op,
      ...(request.op === 'set' || request.op === 'add' ? { value: request.value } : {}),
    }));
    const sent = connection.settingsEdit?.(document.fingerprint, wire) ?? false;
    if (!sent) {
      refusal = {
        reason: 'refused',
        message: 'the connection is not open, so nothing was sent',
        keyPath: null,
      };
      return;
    }
    saving = true;
    refusal = null;
    savedFrom = document.fingerprint;
    savedAgainst = { document, requests };
    clearTimer();
    timer = setTimeout(() => {
      timer = null;
      if (!saving) return;
      saving = false;
      refusal = {
        reason: 'refused',
        message: 'no reload arrived after the save; the file may not have changed',
        keyPath: null,
      };
    }, 5000);
  }

  /** What a removal would leave behind, in the words the confirm draws. */
  function depends(entry: { group: string; label: string; parent: string | null }): string {
    if (entry.group === 'project') {
      const project = entry.label.split(' / ')[1] ?? '';
      const seats =
        home.wire?.agents.filter(
          (agent) =>
            agent.slot.org === entry.parent &&
            agent.slot.project === project &&
            agent.lifecycle !== 'Sleeping',
        ) ?? [];
      if (seats.length === 0) return 'no live sessions; the save removes it at once';
      return `${seats.length} live session${seats.length === 1 ? '' : 's'} (${seats
        .map((agent) => agent.slot.label)
        .join(', ')}); the save will refuse until they close`;
    }
    if (entry.group === 'account') {
      const row = home.wire?.accounts.loading.find((held) => held.display_name === entry.label);
      if (row?.state === 'loading') {
        return 'its usage probe is still running; the save will refuse until it settles';
      }
      return 'sessions already bound to it keep running on its credential until they end';
    }
    if (entry.group === 'slack') {
      return 'its pump keeps sweeping on the old token until a server restart';
    }
    return 'its parameters stop applying at the next spawn';
  }

  function toggleEditing(): void {
    editing = !editing;
    if (!editing) discard();
  }

  function addNew(area: string, group: string, parent: string | null): void {
    const held = frame;
    if (held.status !== 'ready') return;
    addEntry(held.document, draft, area, group, parent);
  }
</script>

<main class="wrap settings-page">
  {#if frame.status === 'waiting'}
    <p class="settings-pending">Reading the effective config...</p>
  {:else if frame.status === 'failed'}
    <p class="settings-pending">{frame.detail}</p>
  {:else}
    <SettingsBody
      document={frame.document}
      {mark}
      {cliVersion}
      {selected}
      onSelect={(id: string) => {
        selected = id;
      }}
      {editing}
      {draft}
      onToggleEditing={toggleEditing}
      onValue={(key: SettingsKey, value: unknown) => setValue(draft, key, value)}
      onEntryValue={(path: string, field: string, value: unknown) =>
        setEntryValue(draft, path, field, value)}
      onAdd={addNew}
      onRemoveRequest={(entry: EntryTarget) => {
        confirm = entry;
      }}
      onCancelAdd={(path: string) => {
        delete draft.adds[path];
      }}
      onUndoRemove={(path: string) => undoRemove(draft, path)}
      {restart}
      errorPath={refusal?.keyPath ?? null}
      onDismissRestart={() => {
        restart = [];
      }}
    />

    {#if editing && (dirty > 0 || saving || refusal !== null)}
      <div class="settings-savebar" role="region" aria-label="unsaved changes">
        <span class="count">
          {dirty} unsaved
        </span>
        {#if refusal !== null}
          <span class="said" class:bad={refusal.reason !== 'stale'}>
            {refusal.message}
          </span>
          {#if refusal.reason === 'stale'}
            <button type="button" class="settings-btn" onclick={() => location.reload()}>
              reload page
            </button>
          {/if}
        {/if}
        <span class="spacer"></span>
        <button type="button" class="settings-btn ghost" onclick={discard}>discard</button>
        <button
          type="button"
          class="settings-btn go"
          disabled={saving || dirty === 0}
          onclick={save}
        >
          {saving ? 'saving...' : 'save'}
        </button>
      </div>
    {/if}

    {#if confirm !== null}
      <div class="settings-scrim">
        <div
          class="settings-dialog"
          role="dialog"
          aria-modal="true"
          aria-labelledby="settings-confirm-title"
        >
          <h2 class="t" id="settings-confirm-title">Remove {confirm.label}?</h2>
          <p class="s">{depends(confirm)}</p>
          <div class="acts">
            <button type="button" class="settings-btn ghost" onclick={() => (confirm = null)}>
              cancel
            </button>
            <button
              type="button"
              class="settings-btn danger"
              onclick={() => {
                if (confirm !== null) {
                  removeEntry(draft, confirm.area, confirm.group, confirm.path, confirm.label);
                }
                confirm = null;
              }}
            >
              remove
            </button>
          </div>
        </div>
      </div>
    {/if}
  {/if}
</main>

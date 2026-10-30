<script lang="ts">
  import type { HomeRead } from '../home/live';
  import type { Connection } from '../socket';
  import type { SettingsDocument, SettingsEditRequest, SettingsKey } from '../wire/settings';
  import ConfirmDialog from '../components/ConfirmDialog.svelte';
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
   * mode, batches its changes into one save.
   *
   * **The draft has a baseline.** A push while the draft is dirty is another
   * writer's, not this page's: the draft and the fingerprint it was built
   * against are kept and a conflict is drawn, rather than rebasing the edits
   * onto a file the reader never saw. A save carries the baseline, and only
   * the acknowledgement for that save's own `id` clears the draft.
   */
  let { connection, home, mark }: { connection: Connection; home: HomeRead; mark: string | null } =
    $props();

  const frame = $derived($settingsFrame);
  const cliVersion = $derived(home.wire?.cli_version?.installed ?? null);
  const frameFingerprint = $derived(frame.status === 'ready' ? frame.document.fingerprint : null);

  type EntryTarget = {
    area: string;
    group: string;
    path: string;
    label: string;
    parent: string | null;
  };
  /** The one confirm the page has open. */
  type Pending =
    | { kind: 'remove'; entry: EntryTarget }
    | { kind: 'discard'; then: 'stop-editing' | 'back' | 'reload' };

  let selected = $state<string | null>(null);
  let editing = $state(false);
  let draft = $state<Draft>(emptyDraft());
  let saving = $state(false);
  let savingId = $state<number | null>(null);
  /** The file fingerprint the draft was built against. */
  let baseline = $state<string | null>(null);
  /** A foreign push's fingerprint, while the draft is dirty. */
  let conflict = $state<string | null>(null);
  let refusal = $state<{
    reason: 'stale' | 'invalid' | 'refused' | 'io';
    message: string;
    keyPath: string | null;
  } | null>(null);
  let restart = $state<string[]>([]);
  let saved = $state(false);
  /** The save waiting for its reload push, for the restart banner. */
  let saveContext = $state<{
    document: SettingsDocument;
    requests: SettingsEditRequest[];
    from: string;
  } | null>(null);
  let pending = $state<Pending | null>(null);
  /** The added entry whose first field takes the keyboard. */
  let focusAdd = $state<string | null>(null);
  let nextId = 1;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const dirty = $derived(dirtyCount(draft));

  function clearTimer(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  function discard(): void {
    draft = emptyDraft();
    refusal = null;
    conflict = null;
    saved = false;
  }

  // **A push while the draft is dirty does not rebase it.** The draft was
  // built from one file, and folding a second writer's change under it would
  // silently drop whichever edit loses. A clean draft simply takes the new
  // baseline.
  $effect(() => {
    const held = $settingsFrame;
    if (held.status !== 'ready' || !editing) return;
    const fingerprint = held.document.fingerprint;
    if (saving) return;
    // A push after a save carries the restart-class keys it landed.
    if (saveContext !== null && fingerprint !== saveContext.from) {
      restart = restartKeys(saveContext.document, held.document, saveContext.requests);
      saveContext = null;
    }
    if (dirty) {
      if (baseline !== null && fingerprint !== baseline) conflict = fingerprint;
      return;
    }
    baseline = fingerprint;
    conflict = null;
  });

  // **Only this save's answer clears the draft.** A push is not proof that
  // the save landed - another client's save pushes too - so the `id` the
  // server echoes is what says which request an answer belongs to.
  $effect(() => {
    const stop = connection.onMessage((message) => {
      if (message.kind === 'settings_saved') {
        if (message.id !== savingId) return;
        saving = false;
        savingId = null;
        clearTimer();
        // The push may already have landed while the answer was in flight;
        // otherwise the next push carries the restart list.
        const held = frame;
        if (
          saveContext !== null &&
          held.status === 'ready' &&
          held.document.fingerprint !== saveContext.from
        ) {
          restart = restartKeys(saveContext.document, held.document, saveContext.requests);
          saveContext = null;
        }
        draft = emptyDraft();
        refusal = null;
        conflict = null;
        saved = true;
        baseline = frameFingerprint;
        return;
      }
      if (message.kind === 'settings_refused') {
        if (message.id !== savingId) return;
        saving = false;
        savingId = null;
        clearTimer();
        refusal = {
          reason: message.reason,
          message: message.message,
          keyPath: message.key_path ?? null,
        };
      }
    });
    return () => {
      stop();
      clearTimer();
    };
  });

  // Saved is a moment rather than a state: the bar says it for a few seconds.
  $effect(() => {
    if (!saved) return;
    const id = setTimeout(() => (saved = false), 4_000);
    return () => clearTimeout(id);
  });

  // Android's Back closes a dirty draft's page with the draft in it: the
  // first change takes a history entry, and a pop asks first.
  let guarded = false;
  $effect(() => {
    if (!editing || !dirty || guarded) return;
    const held: unknown = history.state;
    const state = typeof held === 'object' && held !== null ? held : {};
    history.pushState({ ...state, forgeSettingsDraft: true }, '', location.href);
    guarded = true;
  });
  $effect(() => {
    const onPop = (): void => {
      if (!editing || !dirty) {
        guarded = false;
        return;
      }
      history.pushState({ forgeSettingsDraft: true }, '', location.href);
      guarded = true;
      pending = { kind: 'discard', then: 'back' };
    };
    addEventListener('popstate', onPop);
    return () => removeEventListener('popstate', onPop);
  });

  function save(): void {
    if (frame.status !== 'ready' || dirty === 0 || saving || conflict !== null) return;
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
    const sentFrom = baseline ?? document.fingerprint;
    const id = nextId;
    nextId += 1;
    const sent = connection.settingsEdit?.(sentFrom, wire, id) ?? false;
    if (!sent) {
      refusal = {
        reason: 'refused',
        message: 'the connection is not open, so nothing was sent',
        keyPath: null,
      };
      return;
    }
    saving = true;
    savingId = id;
    refusal = null;
    saved = false;
    saveContext = { document, requests, from: sentFrom };
    clearTimer();
    timer = setTimeout(() => {
      timer = null;
      if (!saving) return;
      saving = false;
      savingId = null;
      refusal = {
        reason: 'refused',
        message: 'no answer arrived after the save; the server did not say whether it wrote',
        keyPath: null,
      };
    }, 5_000);
  }

  /** Leave edit mode, navigate or reload - through one confirm when dirty. */
  function begin(then: 'stop-editing' | 'back' | 'reload'): void {
    if (dirty) {
      pending = { kind: 'discard', then };
      return;
    }
    finish(then);
  }

  function finish(then: 'stop-editing' | 'back' | 'reload'): void {
    discard();
    if (then === 'stop-editing') {
      editing = false;
      baseline = null;
    }
    if (then === 'back') {
      location.assign('/');
      return;
    }
    if (then === 'reload') {
      if (frame.status === 'ready') baseline = frame.document.fingerprint;
      else location.reload();
    }
  }

  function toggleEditing(): void {
    if (saving) return;
    if (editing) begin('stop-editing');
    else {
      editing = true;
      baseline = frameFingerprint;
    }
  }

  function goHome(event: MouseEvent): void {
    if (dirty) {
      event.preventDefault();
      begin('back');
    }
  }

  /** What a removal would leave behind, in the words the confirm draws. */
  function depends(entry: { group: string; label: string; parent: string | null }): string {
    if (entry.group === 'project') {
      if (home.wire === null) {
        return 'its sessions cannot be read yet; the save checks them before writing';
      }
      const project = entry.label.split(' / ')[1] ?? '';
      const seats = home.wire.agents.filter(
        (agent) =>
          agent.slot.org === entry.parent &&
          agent.slot.project === project &&
          agent.lifecycle !== 'Sleeping',
      );
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

  function addNew(area: string, group: string, parent: string | null): void {
    const held = frame;
    if (held.status !== 'ready') return;
    const added = addEntry(held.document, draft, area, group, parent);
    if (added !== null) focusAdd = added.path;
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
      {saving}
      {focusAdd}
      onToggleEditing={toggleEditing}
      onValue={(key: SettingsKey, value: unknown) => setValue(draft, key, value)}
      onEntryValue={(path: string, field: string, value: unknown) =>
        setEntryValue(draft, path, field, value)}
      onAdd={addNew}
      onRemoveRequest={(entry: EntryTarget) => {
        pending = { kind: 'remove', entry };
      }}
      onCancelAdd={(path: string) => {
        delete draft.adds[path];
      }}
      onUndoRemove={(path: string) => undoRemove(draft, path)}
      errorPath={refusal?.keyPath ?? null}
      onBack={goHome}
    />

    {#if editing && (dirty > 0 || saving || refusal !== null || saved || conflict !== null || restart.length > 0)}
      <div class="settings-dock">
        {#if restart.length > 0}
          <div class="settings-restart" role="status">
            <span class="t">saved. these need a server restart:</span>
            <span class="keys">{restart.join(' · ')}</span>
            <button type="button" class="settings-link" onclick={() => (restart = [])}>
              dismiss
            </button>
          </div>
        {/if}
        <div class="settings-savebar" role="region" aria-label="unsaved changes">
          <span class="count">{dirty} unsaved</span>
          {#if saving}<span class="said">saving...</span>{/if}
          {#if saved}<span class="saved">saved</span>{/if}
          {#if conflict !== null}
            <span class="said bad" role="alert"
              >forge.toml changed since this page loaded it; reload to edit it</span
            >
            <button type="button" class="settings-btn" onclick={() => begin('reload')}>
              reload
            </button>
          {/if}
          {#if refusal !== null}
            <span class="said bad" role="alert">{refusal.message}</span>
            {#if refusal.reason === 'stale'}
              <button type="button" class="settings-btn" onclick={() => begin('reload')}>
                reload
              </button>
            {/if}
          {/if}
          <span class="spacer"></span>
          <button type="button" class="settings-btn ghost" disabled={saving} onclick={discard}>
            discard
          </button>
          <button
            type="button"
            class="settings-btn go"
            disabled={saving || dirty === 0 || conflict !== null}
            onclick={save}
          >
            {saving ? 'saving...' : 'save'}
          </button>
        </div>
      </div>
    {/if}

    {#if pending?.kind === 'remove'}
      <ConfirmDialog
        title={`Remove ${pending.entry.label}?`}
        body={depends(pending.entry)}
        confirm="remove"
        onconfirm={() => {
          if (pending?.kind !== 'remove') return;
          removeEntry(
            draft,
            pending.entry.area,
            pending.entry.group,
            pending.entry.path,
            pending.entry.label,
          );
          pending = null;
        }}
        oncancel={() => (pending = null)}
      />
    {:else if pending?.kind === 'discard'}
      <ConfirmDialog
        title="Discard these changes?"
        body="The edits, added entries and removals on this page will be lost."
        confirm="discard"
        onconfirm={() => {
          if (pending?.kind !== 'discard') return;
          const then = pending.then;
          pending = null;
          finish(then);
        }}
        oncancel={() => (pending = null)}
      />
    {/if}
  {/if}
</main>

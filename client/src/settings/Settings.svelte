<script lang="ts">
  import type { HomeRead } from '../home/live';
  import { goTo, holdRoute, type Route } from '../routes';
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
  /** What leaving a dirty draft means on this page. */
  type Then = 'stop-editing' | 'back' | 'reload' | { route: Route };
  /** The one confirm the page has open. */
  type Pending = { kind: 'remove'; entry: EntryTarget } | { kind: 'discard'; then: Then };

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

  /** The bar's one claim, in strict precedence: refusal, conflict, saving,
   *  saved, idle. There is one slot, so two can never be drawn at once. */
  type Claim =
    | { kind: 'idle' }
    | { kind: 'saving' }
    | { kind: 'saved' }
    | { kind: 'conflict' }
    | { kind: 'refusal'; reason: 'stale' | 'invalid' | 'refused' | 'io'; message: string };

  const claim = $derived.by((): Claim => {
    if (refusal !== null)
      return { kind: 'refusal', reason: refusal.reason, message: refusal.message };
    if (conflict !== null) return { kind: 'conflict' };
    if (saving) return { kind: 'saving' };
    if (saved) return { kind: 'saved' };
    return { kind: 'idle' };
  });

  function clearTimer(): void {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  }

  function discard(): void {
    draft = emptyDraft();
    refusal = null;
    conflict = null;
    saved = false;
    // An added entry that is gone takes its focus claim with it.
    focusAdd = null;
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
      // **A foreign push clears the saved flash before it draws.** The flash
      // is the last save's claim; a conflict is this file's, and the bar has
      // one slot, so "saved" must not outlive the push that contradicted it.
      if (baseline !== null && fingerprint !== baseline) {
        conflict = fingerprint;
        saved = false;
      }
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
        focusAdd = null;
        baseline = frameFingerprint;
        return;
      }
      if (message.kind === 'settings_refused') {
        if (message.id !== savingId) return;
        saving = false;
        savingId = null;
        clearTimer();
        // **A refused save wrote nothing.** Its context must not survive to
        // colour a later push as "saved. these need a server restart".
        saveContext = null;
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

  // **One owner of the page's history.** Android's Back closes a dirty
  // draft with a confirm, and a dialog takes an entry of its own so Back
  // closes it; two popstate listeners would answer each other's pops (a
  // cancel reopened the dialog for ever), so this page places and settles
  // every entry it pushes.
  // `$state`, because the effects below must re-run when an entry comes or
  // goes: an acknowledgement landing while a dialog is open clears the draft
  // without changing `dirty` again, and a plain `let` would strand the leave
  // callback.
  let draftEntry = $state(false);
  let dialogEntry = $state(false);
  /** A dialog button's answer, waiting on the entry its own pop removes. */
  let settle: 'confirm' | 'cancel' | null = null;
  let droppingDraft = false;
  let afterDrop: (() => void) | null = null;
  /** What to do once the draft's entry is off the stack. */
  let leave: (() => void) | null = null;

  function heldState(): Record<string, unknown> {
    const held: unknown = history.state;
    return typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
  }

  function pushDraftEntry(): void {
    history.pushState({ ...heldState(), forgeSettingsDraft: true }, '', location.href);
    draftEntry = true;
  }

  /** Drop the draft's entry, then do what the drop was holding up. */
  function dropDraftEntry(after?: () => void): void {
    if (!draftEntry) {
      after?.();
      return;
    }
    droppingDraft = true;
    afterDrop = after ?? null;
    history.back();
  }

  function openPending(next: Pending): void {
    pending = next;
    history.pushState({ ...heldState(), forgeSettingsDialog: true }, '', location.href);
    dialogEntry = true;
  }

  /** A dialog's answer; the pop that removes its entry runs it. */
  function settlePending(answer: 'confirm' | 'cancel'): void {
    if (!dialogEntry) {
      runPending(answer);
      return;
    }
    settle = answer;
    history.back();
  }

  function runPending(answer: 'confirm' | 'cancel'): void {
    const held = pending;
    pending = null;
    if (held === null || answer === 'cancel') return;
    if (held.kind === 'remove') {
      removeEntry(draft, held.entry.area, held.entry.group, held.entry.path, held.entry.label);
      return;
    }
    finish(held.then);
  }

  // The dirty draft keeps one entry; it is dropped the moment nothing is
  // unsaved, so a stray entry does not swallow the next Back.
  $effect(() => {
    if (editing && dirty > 0) {
      if (!draftEntry && !dialogEntry) pushDraftEntry();
      return;
    }
    if (draftEntry && !dialogEntry && !droppingDraft) {
      const after = leave;
      leave = null;
      dropDraftEntry(after ?? undefined);
    }
  });

  $effect(() => {
    const onPop = (): void => {
      if (droppingDraft) {
        droppingDraft = false;
        draftEntry = false;
        const after = afterDrop;
        afterDrop = null;
        after?.();
        return;
      }
      if (settle !== null) {
        const answer = settle;
        settle = null;
        dialogEntry = false;
        runPending(answer);
        return;
      }
      if (pending !== null) {
        // Back closed the dialog; cancelling is what Back means.
        dialogEntry = false;
        runPending('cancel');
        return;
      }
      if (editing && dirty > 0) {
        // Back tried to leave with changes: keep the reader here and ask.
        draftEntry = false;
        pushDraftEntry();
        openPending({ kind: 'discard', then: 'back' });
        return;
      }
      draftEntry = false;
    };
    addEventListener('popstate', onPop);
    return () => removeEventListener('popstate', onPop);
  });

  // A programmatic move - a notification's Open - is the page's to refuse
  // while a draft is unsaved; it asks, then forces the move on a confirm.
  $effect(() => {
    if (!editing || dirty === 0) return;
    return holdRoute((route) => {
      if (pending !== null) return false;
      begin({ route });
      return false;
    });
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
      // An unanswered save must not colour the next push as its own.
      saveContext = null;
      refusal = {
        reason: 'refused',
        message: 'no answer arrived after the save; the server did not say whether it wrote',
        keyPath: null,
      };
    }, 5_000);
  }

  /** Leave edit mode, navigate or reload - through one confirm when dirty. */
  function begin(then: Then): void {
    if (dirty) {
      openPending({ kind: 'discard', then });
      return;
    }
    finish(then);
  }

  function finish(then: Then): void {
    discard();
    if (then === 'stop-editing') {
      editing = false;
      baseline = null;
      return;
    }
    // The move rides the draft entry's own pop, so the history stack is
    // settled before the page leaves it.
    leave =
      then === 'back'
        ? () => location.assign('/')
        : then === 'reload'
          ? () => location.reload()
          : () => goTo(then.route, { force: true });
    // **The effect drops the entry, outside this popstate dispatch.** The
    // entries are `$state`, so a save's answer landing under the open confirm
    // still re-runs it once the dialog entry goes; a nested `back()` here
    // would be issued from inside a traversal. With no entry to drop - a
    // clean page - the move runs now.
    if (!draftEntry) {
      const after = leave;
      leave = null;
      after?.();
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
        openPending({ kind: 'remove', entry });
      }}
      onCancelAdd={(path: string) => {
        delete draft.adds[path];
        // A later add of the same entry must take the keyboard again.
        if (focusAdd === path) focusAdd = null;
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
          {#if claim.kind === 'refusal'}
            <span class="said bad" role="alert">{claim.message}</span>
            {#if claim.reason === 'stale' || conflict !== null}
              <button type="button" class="settings-btn" onclick={() => begin('reload')}>
                reload
              </button>
            {/if}
          {:else if claim.kind === 'conflict'}
            <span class="said bad" role="alert"
              >forge.toml changed since this page loaded it; reload to edit it</span
            >
            <button type="button" class="settings-btn" onclick={() => begin('reload')}>
              reload
            </button>
          {:else if claim.kind === 'saving'}
            <span class="said">saving...</span>
          {:else if claim.kind === 'saved'}
            <span class="saved">saved</span>
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
            save
          </button>
        </div>
      </div>
    {/if}

    {#if pending?.kind === 'remove'}
      <ConfirmDialog
        title={`Remove ${pending.entry.label}?`}
        body={depends(pending.entry)}
        confirm="remove"
        fallback="settings-area-title"
        onconfirm={() => settlePending('confirm')}
        oncancel={() => settlePending('cancel')}
      />
    {:else if pending?.kind === 'discard'}
      <ConfirmDialog
        title="Discard these changes?"
        body="The edits, added entries and removals on this page will be lost."
        confirm="discard"
        fallback="settings-area-title"
        onconfirm={() => settlePending('confirm')}
        oncancel={() => settlePending('cancel')}
      />
    {/if}
  {/if}
</main>

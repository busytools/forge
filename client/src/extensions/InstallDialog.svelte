<script lang="ts">
  /**
   * The install scope picker: where a plugin's install lands.
   *
   * Three scopes, which is the CLI's own set: `user` writes the machine's
   * config, `project` the project's shared settings, `local` the project's
   * local ones. `project` and `local` need a project, so the select is
   * enabled only for them - and the project list is the roster, because a
   * path the server cannot resolve from `forge.toml` is not a target it
   * accepts.
   *
   * It is an overlay, so it takes a history entry (the page pushes it) and
   * the hardware Back closes what it opened. Esc, the backdrop and Cancel
   * close it; focus moves in on open.
   */
  import type { ProjectView } from '../wire/home';

  let {
    plugin,
    projects,
    oninstall,
    oncancel,
  }: {
    /** The plugin being installed, by its display name. */
    plugin: string;
    projects: ProjectView[];
    oninstall: (scope: string, project: string | null) => void;
    oncancel: () => void;
  } = $props();

  let scope = $state<'user' | 'project' | 'local'>('user');
  /** The project the reader picked, or `null` until they do: the first
   * declared project is the default without being latched at open. */
  let picked = $state<string | null>(null);
  const project = $derived(picked ?? projects[0]?.name ?? '');
  let root: HTMLElement | undefined = $state();

  const needsProject = $derived(scope === 'project' || scope === 'local');
  const ready = $derived(!needsProject || project !== '');

  $effect(() => {
    root?.querySelector<HTMLElement>('input')?.focus();
  });

  function key(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.stopPropagation();
      oncancel();
    }
  }

  function confirm(): void {
    if (!ready) return;
    oninstall(scope, needsProject ? project : null);
  }
</script>

<div class="ext-scrim" role="presentation" onclick={oncancel}>
  <!-- The backdrop click is the pointer's way out; the panel keeps its own
       presses from reaching it. -->
  <div
    class="ext-dialog"
    role="dialog"
    aria-modal="true"
    aria-label="Install {plugin}"
    tabindex="-1"
    bind:this={root}
    onclick={(event) => event.stopPropagation()}
    onkeydown={key}
  >
    <div class="ext-dlg-t">Install {plugin}</div>
    <div class="ext-dlg-s">
      A skill is never installed alone: the press installs this plugin and every component it ships.
    </div>

    <div class="ext-scopes" role="radiogroup" aria-label="Scope">
      <label class="ext-scope">
        <input type="radio" name="scope" value="user" data-editor="nowhere" bind:group={scope} />
        <span><b>user</b> - this machine</span>
      </label>
      <label class="ext-scope">
        <input type="radio" name="scope" value="project" data-editor="nowhere" bind:group={scope} />
        <span><b>project</b> - shared in a project's settings</span>
      </label>
      <label class="ext-scope">
        <input type="radio" name="scope" value="local" data-editor="nowhere" bind:group={scope} />
        <span><b>local</b> - a project's own, not committed</span>
      </label>
    </div>

    {#if needsProject}
      {#if projects.length === 0}
        <div class="ext-dlg-note">
          No project is declared in forge.toml, so there is nowhere for this scope to land.
        </div>
      {:else}
        <label class="ext-dlg-field">
          <span>project</span>
          <select value={project} onchange={(event) => (picked = event.currentTarget.value)}>
            {#each projects as entry (entry.key)}
              <option value={entry.name}>{entry.name}</option>
            {/each}
          </select>
        </label>
      {/if}
    {/if}

    <div class="ext-dlg-actions">
      <button type="button" class="btn ghost" onclick={oncancel}>cancel</button>
      <button type="button" class="btn go" disabled={!ready} onclick={confirm}> install </button>
    </div>
  </div>
</div>

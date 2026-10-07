<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { markOf, type GitStats, type StripFile } from '../session/view';
  import { git } from './git.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The working-tree row in the strip above the composer: the branch this
   * seat's tree is on, what moved in it, and the pull request it belongs to.
   *
   * It was the inspector's first section because the working tree is the
   * first thing a person looks for, and the one read whose absence reads as
   * "this seat has no repository" - so this row draws for every seat with a
   * record, which is also what holds the strip open on an idle one.
   */
  let open = $state(false);
  /** The segment and its list, so leaving and opening can be told apart. */
  let segEl: HTMLElement | null = $state(null);
  let listEl: HTMLElement | null = $state(null);
  /** A beat of grace on leaving, so crossing the gap into the list lands. */
  let closing: ReturnType<typeof setTimeout> | null = null;
  /** The panel's measured caps, remeasured on a resize while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    const remeasure = () => (limits = panelStyle(segEl));
    window.addEventListener('resize', remeasure);
    return () => window.removeEventListener('resize', remeasure);
  });

  const strip = $derived(git.strip());

  /**
   * The commit whose own files the reveal is showing. Hover and focus set
   * it, a tap on the row sets it too - the door a finger has, since a
   * touch has no hover.
   */
  let watching = $state<string | null>(null);

  // The reveal goes with the panel: the next open starts quiet.
  $effect(() => {
    if (!open) watching = null;
  });

  /** `N files`, the one-file case spelled out. */
  const filesWord = (stats: GitStats): string =>
    stats.totalFiles === 1 ? '1 file' : `${stats.totalFiles} files`;

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    if (!open) limits = panelStyle(segEl);
    open = true;
  }

  /**
   * Leaving arms the close - the same two guards the agents row carries: a
   * pointer leave whose event carries no related target fires over the
   * panel's gap, and a leave may close while the TOGGLE holds focus but not
   * while a list ROW does.
   */
  function release(event?: FocusEvent) {
    if (event === undefined) {
      if (segEl !== null && segEl.matches(':hover')) return;
      const active = document.activeElement;
      const onRow =
        segEl !== null && listEl !== null && active instanceof Node && listEl.contains(active);
      if (onRow) return;
      arm();
      return;
    }
    const next = event.relatedTarget ?? document.activeElement;
    if (segEl !== null && next instanceof Node && segEl.contains(next)) return;
    arm();
  }

  function arm() {
    if (closing !== null) clearTimeout(closing);
    closing = setTimeout(() => {
      closing = null;
      open = false;
    }, 120);
  }

  /** A pointer that can hover, which a finger cannot. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /** A compatibility press is in flight (the measured chromium tap order). */
  let pointed = false;

  $effect(() => {
    const press = () => (pointed = true);
    const release = () => (pointed = false);
    window.addEventListener('mousedown', press);
    window.addEventListener('mouseup', release);
    window.addEventListener('click', release);
    window.addEventListener('pointercancel', release);
    return () => {
      window.removeEventListener('mousedown', press);
      window.removeEventListener('mouseup', release);
      window.removeEventListener('click', release);
      window.removeEventListener('pointercancel', release);
    };
  });

  function focusIn() {
    if (pointed) {
      pointed = false;
      return;
    }
    hold();
  }

  /** Put focus back on the toggle, which is where a dismissal leaves a reader. */
  function toToggle(): void {
    segEl?.querySelector('button')?.focus();
  }
</script>

{#if strip !== null}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the working tree this seat is on"
      onclick={() => {
        if (open) open = false;
        else hold();
      }}
      onpointerenter={(event) => {
        if (hovering(event)) hold();
      }}
      onpointerleave={(event) => {
        if (hovering(event)) release();
      }}
      onfocusin={focusIn}
      onfocusout={release}
      onkeydown={(event) => {
        if (event.key !== 'Escape') return;
        toToggle();
        open = false;
      }}
    >
      <Icon name="git" />
      <span class="lab">{strip.label}</span>
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        <!-- What the tree IS leads: a fleet's reader needs to know whose
             worktree this is before reading what is in it. -->
        <div class="sg-head">{strip.head}</div>

        {#if strip.ahead !== null}
          {@const said = `${strip.ahead.count} commit${strip.ahead.count === 1 ? '' : 's'}`}
          <div class="sg-head">
            {strip.ahead.base === null ? `${said} ahead` : `${said} ahead of ${strip.ahead.base}`}
          </div>
          {#if strip.ahead.stats !== null && strip.ahead.stats.totalFiles > 0}
            <div class="sg-head">
              {`${filesWord(strip.ahead.stats)} \u{b7} `}{@render figures(strip.ahead.stats)}
            </div>
          {/if}
          {#each strip.ahead.commits as commit (commit.sha)}
            <button
              type="button"
              class="sg-it"
              title={`${commit.sha} \u{b7} ${commit.subject}`}
              onclick={() => {
                // The door a finger has: a tap reveals the commit's own
                // files, which is what a hover shows on a pointer.
                watching = commit.sha;
              }}
              onpointerenter={(event) => {
                if (!hovering(event)) return;
                hold();
                watching = commit.sha;
              }}
              onpointerleave={(event) => {
                if (hovering(event)) release();
              }}
              onfocusin={() => {
                watching = commit.sha;
              }}
              onfocusout={release}
              onkeydown={(event) => {
                if (event.key !== 'Escape') return;
                toToggle();
                open = false;
              }}
            >
              <span class="sha">{commit.sha}</span>
              <span class="nm lead">{commit.subject}</span>
            </button>
            {#if watching === commit.sha && commit.stats !== null}
              <div class="sg-cm">
                {#if commit.stats.files.length > 0}
                  <div class="sg-head">
                    {`${filesWord(commit.stats)} \u{b7} `}{@render figures(commit.stats)}
                  </div>
                  {#each commit.stats.files as file (file.path)}
                    {@render fileRow(file)}
                  {/each}
                {:else}
                  <div class="sg-sub">no files of its own</div>
                {/if}
              </div>
            {/if}
          {/each}
        {/if}

        {#if strip.uncommitted !== null}
          <div class="sg-head">
            {`uncommitted \u{b7} ${filesWord(strip.uncommitted)} \u{b7} `}{@render figures(
              strip.uncommitted,
            )}
          </div>
          {#each strip.uncommitted.files as file (file.path)}
            {@render fileRow(file)}
          {/each}
        {:else if strip.gate === null}
          <div class="sg-head">{`uncommitted \u{b7} clean`}</div>
        {/if}

        {#if strip.pr !== null}
          <!-- A real link, because the PR is a place: the row is the one
               thing in this panel with somewhere to go. -->
          <a
            class="sg-it"
            href={strip.pr.url}
            target="_blank"
            rel="noreferrer"
            onfocusout={release}
            onkeydown={(event) => {
              if (event.key !== 'Escape') return;
              toToggle();
              open = false;
            }}
          >
            <span class="nm">{`PR #${strip.pr.number}`}</span>
            <span class="n">{strip.pr.draft ? 'draft' : 'open'}</span>
          </a>
          {#if strip.pr.closes !== ''}
            <div class="sg-sub">{`closes ${strip.pr.closes}`}</div>
          {/if}
        {/if}

        {#if strip.gate !== null}
          <button
            type="button"
            class="sg-it"
            title={strip.gate}
            onclick={() => {
              open = false;
            }}
            onpointerenter={(event) => {
              if (hovering(event)) hold();
            }}
            onpointerleave={(event) => {
              if (hovering(event)) release();
            }}
            onfocusout={release}
            onkeydown={(event) => {
              if (event.key !== 'Escape') return;
              toToggle();
              open = false;
            }}
          >
            <span class="nm lead">{strip.gate}</span>
          </button>
        {/if}
      </div>
    {/if}
  </span>
{/if}

<!-- One changed file's row, drawn by both the uncommitted list and a
     commit's own reveal: the mark, the path, the counts with their signs. -->
{#snippet fileRow(file: StripFile)}
  {@const mark = markOf(file.status)}
  <button
    type="button"
    class="sg-it"
    title={`${file.path} \u{b7} +${file.added} -${file.removed}`}
    onclick={() => {
      // No destination behind a file yet: the diff page is the later piece.
      open = false;
    }}
    onpointerenter={(event) => {
      if (hovering(event)) hold();
    }}
    onpointerleave={(event) => {
      if (hovering(event)) release();
    }}
    onfocusout={release}
    onkeydown={(event) => {
      if (event.key !== 'Escape') return;
      toToggle();
      open = false;
    }}
  >
    <span class="fm {mark.klass}">{mark.letter}</span>
    <span class="nm lead path">{file.path}</span>
    <span class="n"
      ><span class="plus">{`+${file.added}`}</span><span class="minus">{` -${file.removed}`}</span
      ></span
    >
  </button>
{/snippet}

<!-- A list's totals: the signed counts, plus in green and minus in red. -->
{#snippet figures(stats: GitStats)}
  <span class="fg"
    ><span class="plus">{`+${stats.totalAdded}`}</span><span class="minus"
      >{` -${stats.totalRemoved}`}</span
    ></span
  >
{/snippet}

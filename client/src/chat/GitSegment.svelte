<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { markOf } from '../session/view';
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
      {strip.label}
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
          {#each strip.ahead.commits as commit (commit.sha)}
            <button
              type="button"
              class="sg-it"
              title={`${commit.sha} \u{b7} ${commit.subject}`}
              onclick={() => {
                // No destination behind a commit yet: the row closes the
                // list and that is the whole of it.
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
              <span class="sha">{commit.sha}</span>
              <span class="nm lead">{commit.subject}</span>
            </button>
          {/each}
        {/if}

        {#if strip.files.length > 0}
          <div class="sg-head">
            {`uncommitted \u{b7} ${strip.files.length} file${strip.files.length === 1 ? '' : 's'}`}
          </div>
          {#each strip.files as file (file.path)}
            {@const mark = markOf(file.status)}
            <button
              type="button"
              class="sg-it"
              title={`${file.path} \u{b7} +${file.added} -${file.removed}`}
              onclick={() => {
                // No destination behind a file yet: the diff page is the
                // later piece.
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
              <span class="nm lead">{file.path}</span>
              <span class="n">{`+${file.added} -${file.removed}`}</span>
            </button>
          {/each}
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

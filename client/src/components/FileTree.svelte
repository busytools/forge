<script lang="ts">
  /**
   * One tree's rows, drawn: the review page's own markup, shared, so the
   * review page and the files page draw ONE tree rather than two copies of
   * a row. The derivation is the page's (`components/tree.ts`); this is the
   * drawing. A file row's trailing cells - the review page's stats and
   * comment pill, the files page's comment pill - come from the caller's
   * snippet, so nothing page-specific lives here.
   */
  import type { Snippet } from 'svelte';

  import type { TreeRow } from './tree';

  interface Props {
    rows: TreeRow<unknown>[];
    /** The heading's word ("Files") and the nav's own label ("files in this scope"). */
    heading: string;
    label: string;
    count: number;
    closed: ReadonlySet<string>;
    /** The row marked as where the reader is - the canvas's reading edge, or the open file. */
    here: string | null;
    /** A path whose file was renamed onto it, marked by the caller's own rule. */
    renamed?: (path: string) => boolean;
    ondir: (path: string) => void;
    onfile: (path: string) => void;
    trailing?: Snippet<[TreeRow<unknown>]>;
  }

  let { rows, heading, label, count, closed, here, renamed, ondir, onfile, trailing }: Props =
    $props();

  /**
   * The tree takes the arrows (Ved, 2026-10-17, on the files page's own
   * look - and the review page's tree is the same tree, so both move the
   * same way): up and down walk the visible rows in order, Home and End go
   * to the ends, and left and right fold and unfold a directory. Tab stays
   * the way in, exactly as the buttons already are.
   */
  let navEl: HTMLElement | null = $state(null);

  function rowButtons(): HTMLButtonElement[] {
    if (navEl === null) return [];
    return [...navEl.querySelectorAll<HTMLButtonElement>('button.rv-dir, button.rv-frow')];
  }

  function focusRow(from: EventTarget | null, at: number | 'first' | 'last'): void {
    const buttons = rowButtons();
    if (buttons.length === 0) return;
    const index = buttons.indexOf(from as HTMLButtonElement);
    const next =
      at === 'first'
        ? 0
        : at === 'last'
          ? buttons.length - 1
          : Math.min(Math.max(index + at, 0), buttons.length - 1);
    buttons[next]?.focus();
  }

  function onkey(event: KeyboardEvent): void {
    const el = event.target;
    if (!(el instanceof HTMLButtonElement)) return;
    switch (event.key) {
      case 'ArrowDown':
        event.preventDefault();
        focusRow(el, 1);
        break;
      case 'ArrowUp':
        event.preventDefault();
        focusRow(el, -1);
        break;
      case 'Home':
        event.preventDefault();
        focusRow(el, 'first');
        break;
      case 'End':
        event.preventDefault();
        focusRow(el, 'last');
        break;
      // A directory folds and unfolds; a file's left and right are its own.
      case 'ArrowRight':
        if (el.classList.contains('rv-dir') && el.getAttribute('aria-expanded') === 'false') {
          event.preventDefault();
          ondir(el.dataset.path ?? '');
        }
        break;
      case 'ArrowLeft':
        if (el.classList.contains('rv-dir') && el.getAttribute('aria-expanded') === 'true') {
          event.preventDefault();
          ondir(el.dataset.path ?? '');
        }
        break;
      default:
        break;
    }
  }
</script>

<!-- The keydown relays the arrows between the buttons beneath it; the
     buttons are the interactive elements, and Tab still enters by them. -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<nav class="rv-files" aria-label={label} bind:this={navEl} onkeydown={onkey}>
  <h2 class="rv-h">{heading} <span class="rv-n">{count}</span></h2>
  {#each rows as row (row.kind + ':' + row.path)}
    {#if row.kind === 'dir'}
      <button
        type="button"
        class="rv-dir"
        style="--d:{row.depth}"
        data-path={row.path}
        aria-expanded={!closed.has(row.path)}
        onclick={() => ondir(row.path)}
      >
        <span class="rv-arw" class:open={!closed.has(row.path)}>&#9656;</span>
        <span class="rv-dirname">{row.name}/</span>
        <span class="rv-dircount">{row.count}</span>
      </button>
    {:else}
      <button
        type="button"
        class="rv-frow"
        class:renamed={renamed !== undefined && renamed(row.path)}
        class:here={here === row.path}
        aria-current={here === row.path}
        style="--d:{row.depth}"
        title={row.path}
        onclick={() => onfile(row.path)}
      >
        <span class="rv-fpath">{row.name}</span>
        {#if trailing}
          {@render trailing(row)}
        {/if}
      </button>
    {/if}
  {/each}
</nav>

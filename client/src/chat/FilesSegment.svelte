<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { files } from './files.svelte';
  import { panelStyle, portal } from './strip-panel';

  /**
   * The files slip in the strip above the composer: the seat's whole tree,
   * and the door to the page that reads it.
   *
   * **Mandatory, by the ruling (Ved, 2026-10-17)**: every seat has a tree -
   * a worker's own worktree, the lead's checkout - so this slip draws for
   * every seat with a record, where the tree row beside it keeps its own
   * rule of staying away when nothing has moved.
   *
   * **The toggle IS the door** (Ved, 2026-10-17, second ruling): clicking
   * it opens the files page - it reads `walk · files` for a worker (the
   * worktree's own name, so the destination is never a guess) and plain
   * `files` for the project's checkout, with the browser's own mark beside
   * it and no count. On a pointer that can hover the panel still peeks open
   * (the tree's path, and the door again for a keyboard reader); a tap goes
   * straight there.
   *
   * The scaffold - hover grace, the portal, the pointer's own compat - is
   * the strip's own, as every segment's is.
   */
  let open = $state(false);
  /** Where this seat's files page lives, handed in by the page that owns the seat. */
  let { href = null }: { href?: string | null } = $props();
  /** The segment and its list, so leaving and opening can be told apart. */
  let segEl: HTMLElement | null = $state(null);
  let listEl: HTMLElement | null = $state(null);
  /** A beat of grace on leaving, so crossing the gap into the list lands. */
  let closing: ReturnType<typeof setTimeout> | null = null;
  /** The panel's measured caps, remeasured every frame while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(segEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  const tree = $derived(files.tree());
  const name = $derived(files.name());

  /** The toggle names the tree it opens: the worktree's own name, else the
   *  noun - and the icon says the kind. */
  const label = $derived(name === null ? 'files' : `${name} · files`);

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    if (!open) limits = panelStyle(segEl);
    open = true;
  }

  function release(event?: FocusEvent) {
    if (event === undefined) {
      if (segEl !== null && segEl.matches(':hover')) return;
      // The panel is portaled to the body (`use:portal`), so moving off the
      // segment into the panel is not a departure.
      if (listEl !== null && listEl.matches(':hover')) return;
      const active = document.activeElement;
      const onRow =
        segEl !== null && listEl !== null && active instanceof Node && listEl.contains(active);
      if (onRow) return;
      arm();
      return;
    }
    const next = event.relatedTarget ?? document.activeElement;
    // The panel is portaled to the body (`use:portal`), so focus moving INTO
    // it is not a departure - the segment can no longer contain it.
    if (
      next instanceof Node &&
      ((segEl !== null && segEl.contains(next)) || (listEl !== null && listEl.contains(next)))
    ) {
      return;
    }
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
    const releasePress = () => (pointed = false);
    window.addEventListener('mousedown', press);
    window.addEventListener('mouseup', releasePress);
    window.addEventListener('click', releasePress);
    window.addEventListener('pointercancel', releasePress);
    return () => {
      window.removeEventListener('mousedown', press);
      window.removeEventListener('mouseup', releasePress);
      window.removeEventListener('click', releasePress);
      window.removeEventListener('pointercancel', releasePress);
    };
  });

  function focusIn() {
    if (pointed) {
      pointed = false;
      return;
    }
    hold();
  }
</script>

{#if files.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    {#if href !== null}
      <!-- The toggle is the door: a press goes to the page, never a toggle. -->
      <a
        class="sg-tog"
        {href}
        aria-label={name === null ? "the seat's own files" : `the seat's files, in ${name}`}
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
          open = false;
        }}
      >
        <Icon name="files" />
        <span>{label}</span>
      </a>
    {:else}
      <span class="sg-tog">
        <Icon name="files" />
        <span>{label}</span>
      </span>
    {/if}

    {#if open}
      <div
        class="sg-list"
        bind:this={listEl}
        style={limits}
        use:portal
        role="presentation"
        onpointerenter={(event) => {
          if (hovering(event)) hold();
        }}
        onpointerleave={(event) => {
          if (hovering(event)) release();
        }}
      >
        {#if href !== null}
          <!-- The door onto the page that draws the tree, the reader and the
               comments - drawn apart from the rows, so it is never taken for
               one of them. -->
          <a class="sg-open" {href} onclick={() => (open = false)}>open files {'\u{2192}'}</a>
        {/if}
        {#if tree !== null}
          <div class="sg-head">this tree: {tree}</div>
        {/if}
        <div class="sg-sub">
          the whole tree, held for every seat - a worker's worktree, the lead's checkout.
        </div>
      </div>
    {/if}
  </span>
{/if}

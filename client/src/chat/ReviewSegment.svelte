<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { review } from './review.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The review row in the strip above the composer: what this seat's branch
   * has under review, and the door to the page that draws it.
   *
   * **The strip is how the page is reached**, not only its URL: the row says
   * how many answers wait to be read - the live fact a reviewer is looking
   * for - and the panel lists the threads and opens the page. It draws only
   * while the page has something behind it: a thread, or the branch's own
   * changes.
   */
  let open = $state(false);
  /** Where this seat's review lives, handed in by the page that owns the seat. */
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

  const waiting = $derived(review.waiting());
  const rows = $derived(review.rows());

  /** The toggle's own words: what waits, else what is open, else the noun. */
  const label = $derived(
    waiting > 0
      ? `${waiting} to read`
      : rows.length > 0
        ? `${rows.length} thread${rows.length === 1 ? '' : 's'}`
        : 'review',
  );

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    if (!open) limits = panelStyle(segEl);
    open = true;
  }

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

{#if review.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the reviews on this seat's branch"
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
      <Icon name="review" />
      <span class:needs={waiting > 0}>{label}</span>
    </button>

    {#if open}
      <div
        class="sg-list"
        bind:this={listEl}
        style={limits}
        role="presentation"
        onpointerenter={(event) => {
          if (hovering(event)) hold();
        }}
        onpointerleave={(event) => {
          if (hovering(event)) release();
        }}
      >
        {#if href !== null}
          <!-- The way onto the page that draws the diff, its threads and the
               writing: a door rather than a row, drawn apart from the thread
               rows so it is never taken for one of them. -->
          <a class="sg-open" {href} onclick={() => (open = false)}>open review {'\u{2192}'}</a>
        {/if}
        {#if rows.length === 0}
          <div class="sg-head">nothing under review on this branch yet</div>
        {/if}
        {#each rows as row (row.id)}
          <div class="sg-grp" role="presentation">
            <div class="sg-it">
              {#if row.waiting}<span class="wait"></span>{/if}
              <span class="nm lead">{row.where}</span>
              <span class="n" class:needs={row.waiting}>{row.state}</span>
            </div>
            {#if row.words !== ''}
              <div class="sg-sub">{row.words}</div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </span>
{/if}

<style>
  .needs {
    color: var(--warn);
  }
</style>

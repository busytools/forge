<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import { review } from './review.svelte';
  import { panelStyle, portal } from './strip-panel';

  /**
   * The review row in the strip above the composer: what this seat's branch
   * has under review, and the door to the page that draws it.
   *
   * **The strip is how the page is reached**, not only its URL: the row says
   * how many answers wait to be read - the live fact a reviewer is looking
   * for - and the panel lists the threads and opens the page. It draws
   * wherever the tree row draws (#2203), with no thread lines when nothing
   * is filed; a threads read that did not land draws as itself.
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
  /** The threads the branch has: the read's own row is not one of them. */
  const count = $derived(rows.filter((row) => !row.synthetic).length);

  /** The filed rounds, oldest first - the conversation's own direction. */
  const sets = $derived(review.sets());
  /** Which threads the reader has opened, by id: the panel's own visit. */
  const opened = new SvelteSet<string>();

  function toggleThread(id: string): void {
    if (opened.has(id)) opened.delete(id);
    else opened.add(id);
  }

  /** The toggle's own words: what waits, else what is open, else the noun. */
  const label = $derived(
    waiting > 0
      ? `${waiting} to read`
      : count > 0
        ? `${count} thread${count === 1 ? '' : 's'}`
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
          <!-- The way onto the page that draws the diff, its threads and the
               writing: a door rather than a row, drawn apart from the thread
               rows so it is never taken for one of them. -->
          <a class="sg-open" {href} onclick={() => (open = false)}>open review {'\u{2192}'}</a>
        {/if}
        {#if sets.length === 0}
          <div class="sg-head">nothing under review on this branch yet</div>
        {/if}
        {#each sets as set (set.id)}
          {@const unread = set.number === 0}
          <div class="sg-grp" role="presentation">
            <div class="sg-it">
              <span class="nm lead">{unread ? 'not read' : `R${set.number}`}</span>
              {#if set.age !== ''}<span class="n">{set.age}</span>{/if}
            </div>
            {#if set.summary !== null && set.summary !== ''}
              <div class="sg-sub">{set.summary}</div>
            {/if}
            {#if set.chips.length > 0}
              <div class="sg-chips">
                {#each set.chips as chip (chip.state)}
                  <span class="chip {chip.state}">{chip.count} {chip.state}</span>
                {/each}
              </div>
            {/if}
            {#if unread}
              <!-- A read that did not land draws as itself, its reason at
                   rest: there are no rounds to expand. -->
              <div class="sg-sub">{set.threads[0]?.turns[0]?.text ?? ''}</div>
            {:else}
              {#each set.threads as thread (thread.id)}
                <button
                  type="button"
                  class="sg-thread"
                  aria-expanded={opened.has(thread.id)}
                  onclick={() => toggleThread(thread.id)}
                >
                  <span class="nm">{thread.where}</span>
                  <span class="n {thread.state}">{thread.state}</span>
                </button>
                {#if opened.has(thread.id)}
                  {#if thread.code.length > 0}
                    <pre class="sg-code" class:captured={!thread.inDiff}>{thread.code.join(
                        '\n',
                      )}</pre>
                  {/if}
                  {#each thread.turns as turn, i (i)}
                    <div class="sg-turn">
                      {#if turn.author !== ''}<span class="who">{turn.author}</span>{/if}
                      <div class="sg-sub">{turn.text}</div>
                    </div>
                  {/each}
                {/if}
              {/each}
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

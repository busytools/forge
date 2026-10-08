<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { tasks } from './tasks.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The tasks row in the strip above the composer: what this seat's project
   * holds, how far along each is, and who holds it.
   *
   * **A task that moved lights its row for a beat.** The read this row serves
   * is knowing that something happened without watching the list, so a status
   * change - or a task arriving - tints the row the same way the reveal tints
   * the row it lands on, and the tint clears itself.
   */
  let open = $state(false);
  /**
   * Where this project's board lives, handed in by the page that owns the
   * seat. The strip is the door: the pill is the count, the list's first
   * line opens the board itself.
   */
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
    // **While the panel is open it follows the layout it hangs in.** The
    // style is measured once at the open, and an ordinary reflow - a
    // segment's live figures re-wrapping, the composer growing and lifting
    // the strip, a new segment appearing beside it - would otherwise leave
    // the panel where the old geometry put it, off the screen again. A frame
    // loop while the panel is open (it closes on leave) re-measures; the
    // write lands only when the string changes.
    let raf = requestAnimationFrame(function tick() {
      limits = panelStyle(segEl);
      raf = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(raf);
  });

  const count = $derived(tasks.count());

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

{#if tasks.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the tasks this project holds"
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
      <Icon name="tasks" />
      {`${count.done} of ${count.total}`}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#if href !== null}
          <!-- The way into the whole board, in the strip the composer
               carries. A door rather than a row: it is drawn apart from the
               task rows so it is never taken for one of them. -->
          <a class="sg-open" {href} onclick={() => (open = false)}>open board {'\u{2192}'}</a>
        {/if}
        {#each tasks.rows() as row (row.id)}
          <div
            class="sg-grp"
            role="presentation"
            onpointerenter={(event) => {
              if (hovering(event)) hold();
            }}
            onpointerleave={(event) => {
              if (hovering(event)) release();
            }}
          >
            <button
              type="button"
              class="sg-it"
              class:settled={row.status === 'completed'}
              class:hit={tasks.lit(row.id)}
              title={row.owner === null ? row.meta : `${row.owner} \u{b7} ${row.meta}`}
              onclick={() => {
                // No destination behind a task yet: the row closes the list
                // and that is the whole of it.
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
              {#if row.status === 'in_progress'}
                <span class="ring"></span>
              {:else if row.status === 'waiting'}
                <span class="wait"></span>
              {:else if row.status === 'failed'}
                <Icon name="x" class="bad" />
              {:else if row.status === 'canceled'}
                <Icon name="x" class="off" />
              {:else if row.status === 'completed'}
                <Icon name="check" class="ok" />
              {:else}
                <span class="hollow"></span>
              {/if}
              <span class="nm lead">{row.display}</span>
              {#if row.owner !== null}
                <span class="n">{row.owner}</span>
              {/if}
            </button>
            <div class="sg-sub">{row.meta}</div>
          </div>
        {/each}
      </div>
    {/if}
  </span>
{/if}

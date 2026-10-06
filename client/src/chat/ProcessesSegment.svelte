<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { askReveal } from '../session/scroll-ask';
  import { memoryLabel } from '../session/view';
  import { processes, type ProcessRow } from './processes.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The batch row in the strip above the composer: the seat's backgrounded
   * work, agents-style - how many calls are running, what they hold together,
   * and the way into each.
   *
   * **The registry names the rows; the walk supplies the figures.** A row is
   * a backgrounded call, led by the CLI's own description, and adopts the
   * scanned process its command names for memory and pid - the terminal's
   * join, ported, read from one store so the glance and the list cannot
   * disagree. A call the walk missed still draws, without a figure, wearing
   * its kind tag. Nothing else walks in: MCP servers, grandchildren and
   * detached processes are not batch work.
   *
   * **Nothing here pops.** A row leaving the registry is the settlement - it
   * is kept to the turn's close and nothing flashes for it. What rides along
   * is the agents row's own marks: the work spinner while anything is in
   * flight, the check once everything has settled.
   */

  /** Whether the list is open: hover, tap, focus and Escape all speak to it. */
  let open = $state(false);
  /** The segment and its list, so leaving and opening can be told apart. */
  let segEl: HTMLElement | null = $state(null);
  let listEl: HTMLElement | null = $state(null);
  /** A beat of grace on leaving, so crossing the gap into the list lands. */
  let closing: ReturnType<typeof setTimeout> | null = null;

  const live = $derived(processes.rows());
  /**
   * The order the open list shows.
   *
   * The store sorts by memory and the walk moves every beat, so a live order
   * would re-shuffle under the pointer. Held from the open, and refreshed
   * only while the list is shut.
   */
  let held: ProcessRow[] | null = $state(null);
  const listed = $derived(held ?? live);
  /** The panel's measured caps, remeasured on a resize while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    const remeasure = () => (limits = panelStyle(segEl));
    window.addEventListener('resize', remeasure);
    return () => window.removeEventListener('resize', remeasure);
  });

  const running = $derived(processes.running());
  const settled = $derived(processes.settledCount());
  const total = $derived(processes.totalBytes());

  /**
   * The toggle's one line, joined rather than interleaved with blocks.
   *
   * **Svelte trims whitespace at a block's start**, so a separator written
   * inside its own `{#if}` lost the space before it and the row read
   * `2 running\u{b7} 1.8 GB`. The agents sibling joins its string for the
   * same reason, and its test pins the spaced form.
   */
  const label = $derived.by(() => {
    const parts = [`${running} running`];
    if (total > 0) parts.push(memoryLabel(total));
    if (settled > 0) parts.push(`${settled} settled`);
    return parts.join(' \u{b7} ');
  });

  /** A row's right-hand figure, joined for the same reason; the kind stands
   *  where nothing was measured. */
  const figureOf = (row: ProcessRow): string => {
    if (row.ended_ms !== null) return `ended ${endedAt(row.ended_ms)}`;
    const parts: string[] = [];
    if (row.memory_bytes !== null) parts.push(memoryLabel(row.memory_bytes));
    if (row.pid !== null) parts.push(`pid ${row.pid}`);
    return parts.length > 0 ? parts.join(' \u{b7} ') : row.kind;
  };

  /** When a settled call ended, as a plain clock time. */
  const endedAt = (ms: number) =>
    new Date(ms).toLocaleTimeString(undefined, {
      hour: '2-digit',
      minute: '2-digit',
      hour12: false,
    });

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    if (!open) {
      held = live;
      limits = panelStyle(segEl);
    }
    open = true;
  }

  /**
   * Leaving arms the close.
   *
   * **Pointer and keyboard leave differently**, the same two guards the
   * agents row carries: a pointer leave whose event carries no related target
   * fires over the list's own gap, and a leave may close while the TOGGLE
   * holds focus but not while a list ROW does - closing would unmount the row
   * the reader is on.
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
      held = null;
    }, 120);
  }

  /** A pointer that can hover, which a finger cannot: a tap's synthesised
   *  enter must not arm the hover path, or the click that follows toggles the
   *  list straight back shut. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /**
   * A compatibility press is in flight.
   *
   * **A tap focuses the toggle through its compat `mousedown`** (measured in
   * chromium: pointerdown and pointerup both complete before mousedown, the
   * focus and the click), so focus opens the list only while a press is in
   * flight; `mouseup` ends every quiet press.
   */
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

  /** The toggle took focus: opening from it, unless a press put it there. */
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

{#if processes.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the processes this seat has running"
      onclick={() => (open ? ((open = false), (held = null)) : hold())}
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
        // Focus first: the focusin that follows sets `open` back, so closing
        // after it is the close that sticks.
        toToggle();
        open = false;
        held = null;
      }}
    >
      <Icon name="processes" />
      <!-- The mark mirrors the agents row: the spinner means work in flight,
           and a seat whose batch work has all settled wears the check
           instead of a spinner over a "0 running". -->
      {#if running > 0}
        <span class="ring" aria-hidden="true"></span>
      {:else}
        <Icon name="check" class="ok" />
      {/if}
      {label}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#each listed as row (row.key)}
          <button
            type="button"
            class="sg-it"
            class:settled={row.settled}
            title={row.command}
            onclick={() => {
              // The row's own call is where the detail lives: reveal closes
              // the list and takes the column to the Bash row, which opens
              // and flashes. A call forge never saw a card for has no row to
              // reach, so the click closes and nothing more is promised.
              if (row.tool_use_id !== null) askReveal(row.tool_use_id);
              open = false;
              held = null;
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
              held = null;
            }}
          >
            {#if row.settled}
              {#if row.failed}
                <Icon name="x" class="bad" />
              {:else}
                <Icon name="check" class="ok" />
              {/if}
            {/if}
            <span class="nm">{row.headline}</span>
            <span class="n">{figureOf(row)}</span>
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}

<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { schedules } from './schedules.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The schedules row in the strip above the composer: this seat's crons in
   * the project, when each is next due, and nothing when there are none.
   *
   * **A cron is owned by the seat that created it** (`team_role`, `None` for
   * the lead), so the row keeps its own label's set, like the connectors row
   * beside it. Nothing pops here: Ved ruled the row need only show and open,
   * so a schedule appearing or leaving simply changes the list, and the rows
   * are inert (no destination behind a schedule).
   */

  /** Whether the list is open: hover, tap, focus and Escape all speak to it. */
  let open = $state(false);
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

  const count = $derived(schedules.count());

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

{#if schedules.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the schedules this seat created"
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
      <Icon name="schedules" />
      {`${count} schedule${count === 1 ? '' : 's'}`}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#each schedules.rows() as row (row.id)}
          <button
            type="button"
            class="sg-it"
            title={`${row.key} \u{b7} ${row.value}`}
            onclick={() => {
              // No destination behind a schedule: the row closes the list and
              // that is the whole of it.
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
            <!-- The key wears the accent the sibling rows use for the name
                 they lead with, and takes the row: a cron's own description
                 is a sentence, so the key elides and the countdown keeps
                 the right edge. -->
            <span class="nm lead">{row.key}</span>
            <span class="n">{row.value}</span>
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}

<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { connectors } from './connectors.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The connectors row in the strip above the composer: the seat's own
   * connector subscriptions - Gotify apps and Slack targets - and nothing
   * when it holds none.
   *
   * **Each seat reads its own.** The sets cross per project with a `team_role`
   * owner on every subscription, so the lead's page reads the no-owner set and
   * a worker's page its own label's; the filter lives in `seatConnectorRows`.
   *
   * **Nothing pops here, deliberately.** A subscription change already lands
   * in the chat when a message arrives, and Ved ruled the segment need only
   * show and open - so no mark, no flash, no click-through: the rows are the
   * detail.
   */

  /** Whether the list is open: hover, tap, focus and Escape all speak to it. */
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

  const count = $derived(connectors.count());

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

{#if connectors.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the connector subscriptions this seat holds"
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
      <!-- BOTH marks lead the toggle: it is the pair's subject, and the bell
           alone would draw identically to a gotify row (same sprite path). -->
      <Icon name="gotify" />
      <Icon name="slack" />
      {`${count} subscription${count === 1 ? '' : 's'}`}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#each connectors.rows() as row (`${row.kind}:${row.id}`)}
          <button
            type="button"
            class="sg-it"
            title={`${row.key} \u{b7} ${row.value}`}
            onclick={() => {
              // No destination behind a subscription: the row closes the
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
            <!-- The connector's own glyph leads, so a glance tells slack from
                 gotify without reading the row. The key elides like the
                 siblings' middle column: an applications list can run long. -->
            <Icon name={row.kind} />
            <span class="tx">{row.key}</span>
            <span class="n">{row.value}</span>
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}

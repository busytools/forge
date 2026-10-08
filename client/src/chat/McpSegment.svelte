<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { mcp } from './mcp.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The MCP row in the strip above the composer: the servers this session's
   * bridge reported, each with the status depth the wire carries - the tools a
   * connected server offers, the command or URL backing it, the reason a
   * failed one gives.
   *
   * **The row shows as much as the wire has, because this is where the status
   * lives now.** The inspector's section said the same thing in fewer words;
   * the rows are inert for now (a pick closes the list), and the run-a-command
   * page reads off them later.
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

  /** The servers themselves: the read's own failure row is not one of them. */
  const count = $derived(mcp.rows().filter((row) => !row.synthetic).length);

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

{#if mcp.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the MCP servers this session reported"
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
      <Icon name="mcp" />
      {count === 0 ? 'MCP failed' : `${count} MCP${count === 1 ? '' : 's'}`}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        <!-- Rows key on the server's name, which is unique in a session's
             own snapshot. -->
        {#each mcp.rows() as row (row.name)}
          <!-- The group holds while the pointer is anywhere inside it, the
               sub-lines included: they carry no row of their own, and a
               pointer entering the panel over one is landing on the panel.
               `role="presentation"` is what the compiler's a11y check asks
               of a plain div carrying pointer handlers. -->
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
              title={`${row.k} \u{b7} ${row.v}`}
              onclick={() => {
                // No destination yet: the row closes the list, and the
                // run-a-command page hangs off it later.
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
              <span class="nm lead">{row.k}</span>
              <span class="n">{row.v}</span>
            </button>
            {#if row.command !== null}
              <!-- A remote server is reached rather than run, and the URL
                   says which without a second field. -->
              {@const line = `${row.command.startsWith('http') ? 'reaches' : 'runs'} ${row.command}`}
              <div class="sg-sub">{line}</div>
            {/if}
            {#if row.tools.length > 0}
              {@const line = `tools ${row.tools.join(', ')}`}
              <div class="sg-sub">{line}</div>
            {/if}
            {#if row.reason !== null}
              <div class="sg-sub bad">{row.reason}</div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </span>
{/if}

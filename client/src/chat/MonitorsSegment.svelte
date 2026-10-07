<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { monitors } from './monitors.svelte';
  import { panelStyle } from './strip-panel';

  /**
   * The monitors row in the strip above the composer: the commands this
   * session is watching, and how each stands.
   *
   * Monitors live here and not in the chat, so this row is the surface that
   * says what a session is watching. The output under a monitor is not here:
   * a tail is read off the machine running forge, and this client does not
   * read the server's disk.
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

  const running = $derived(monitors.rows().filter((row) => row.running).length);

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

{#if monitors.anything()}
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      aria-label="the commands this session is watching"
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
      <Icon name="monitors" />
      {`${running} running`}
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#each monitors.rows() as row (row.id)}
          {@const line = `$ ${row.command}`}
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
              class:settled={!row.running}
              title={`${row.name} \u{b7} ${row.label}`}
              onclick={() => {
                // No destination behind a monitor: the row closes the list
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
              {#if row.running}
                <span class="ring"></span>
              {:else}
                <Icon name="check" class="ok" />
              {/if}
              <span class="nm lead">{row.name}</span>
              <span class="n">{row.label}</span>
            </button>
            <div class="sg-sub">{line}</div>
          </div>
        {/each}
      </div>
    {/if}
  </span>
{/if}

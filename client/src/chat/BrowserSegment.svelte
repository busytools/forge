<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { canHost, closeContext, listContexts, whyText, type ContextRow } from '../browser/host';
  import type { Connection } from '../socket';

  /**
   * The browser row in the strip above the composer: how many named contexts
   * this client holds, and - opened - who owns each, which client drives
   * them, and the controls the person has.
   *
   * **A segment beside the agents row, not a page of its own.** The inspector
   * is going, and this is the shape its survivors take: a row in the strip,
   * its list opening in place (the mockup Ved settled, 2026-10-06). When the
   * surviving strip lands, this mounts into it unchanged.
   *
   * Two controls live here and each is the person's, never a session's:
   * **Take over** (the force override, where a click can honestly serve it)
   * and a context row's **close** (which is what makes a context whose
   * owning session is gone recoverable).
   *
   * **No visible toggle lives here any more.** Seeing the browser is the
   * hand-off's own Open, which shows it inside this app; a control that
   * raised an operating-system window was the machinery that replacement
   * retired, and a second door to a different browser is a lie about which
   * one the sessions drive.
   *
   * The contexts are the CLIENT's own state - it owns the drivers - so the
   * row reads them from the host it runs in, never from the server.
   */
  let { connection, capable = canHost() }: { connection: Connection; capable?: boolean } = $props();

  let open = $state(false);
  /** The strip's own snapshot: read at mount and when the list opens, not per frame. */
  let contexts = $state<ContextRow[]>([]);
  /**
   * What the last read answered. "No contexts yet" is a claim about this
   * client's own state, and a read that never answered has no state to claim,
   * so the row waits - or says why - rather than asserting one.
   */
  let read = $state<'loading' | 'ready' | 'failed'>('loading');
  let why = $state<string | null>(null);
  // The role is read ONCE here and kept live by the subscription below: the
  // connection's identity does not change over this segment's life, so the
  // initial read is the truth the subscription then maintains.
  // svelte-ignore state_referenced_locally
  let hosting = $state(connection.browserRole());
  let segEl = $state<HTMLElement | null>(null);

  $effect(() => connection.onBrowserRole((now) => (hosting = now)));

  /**
   * The client's own contexts, read at mount and when the list opens. A
   * failed read keeps whatever the last one answered and says why.
   */
  async function readContexts(): Promise<void> {
    try {
      contexts = await listContexts();
      read = 'ready';
      why = null;
    } catch (error) {
      read = 'failed';
      why = whyText(error);
    }
  }

  $effect(() => {
    void readContexts();
  });

  /**
   * How the panel closes when the pointer leaves: with a grace period, because
   * a gap sits between the toggle and the panel and a pointer crossing it
   * would otherwise never reach the rows.
   */
  let leaving: ReturnType<typeof setTimeout> | null = null;

  /** Open, and read the contexts fresh, which is what every opening does. */
  function hold(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    open = true;
    void readContexts();
  }

  function arm(): void {
    if (leaving !== null) clearTimeout(leaving);
    leaving = setTimeout(() => {
      leaving = null;
      open = false;
    }, 120);
  }

  /**
   * Leaving arms the close, with the two guards every sibling row carries: a
   * pointer leave arriving over the panel's own gap is not a departure while
   * the segment is still hovered, and a leave may close while the TOGGLE
   * holds focus but not while a panel control does - closing would unmount
   * the control the reader is on.
   */
  function release(event?: FocusEvent): void {
    if (event === undefined) {
      if (segEl !== null && segEl.matches(':hover')) return;
      const active = document.activeElement;
      const toggle = segEl?.querySelector('button') ?? null;
      if (segEl !== null && active instanceof Node && segEl.contains(active) && active !== toggle) {
        return;
      }
      arm();
      return;
    }
    const next = event.relatedTarget ?? document.activeElement;
    if (segEl !== null && next instanceof Node && segEl.contains(next)) return;
    arm();
  }

  /** A pointer that can hover, which a finger cannot: a tap's synthesised
   *  enter must not arm the hover path, or the click that follows toggles the
   *  panel straight back shut. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /**
   * A compatibility press is in flight.
   *
   * **A tap focuses the toggle through its compat `mousedown`**, so focus
   * opens the panel only while a press is in flight; `mouseup` ends every
   * quiet press.
   */
  let pointed = false;

  $effect(() => {
    const press = () => (pointed = true);
    const settle = () => (pointed = false);
    window.addEventListener('mousedown', press);
    window.addEventListener('mouseup', settle);
    window.addEventListener('click', settle);
    window.addEventListener('pointercancel', settle);
    return () => {
      window.removeEventListener('mousedown', press);
      window.removeEventListener('mouseup', settle);
      window.removeEventListener('click', settle);
      window.removeEventListener('pointercancel', settle);
    };
  });

  /** The toggle took focus: opening from it, unless a press put it there. */
  function focusIn(): void {
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

  // A close armed when the segment unmounts would write to a dead instance.
  $effect(() => () => {
    if (leaving !== null) clearTimeout(leaving);
  });

  /**
   * What the collapsed row says this client holds.
   *
   * A count is a claim about a read, so before one has answered - or when the
   * last one failed and nothing was ever read - the row says the count is not
   * known rather than standing on a zero nothing measured. A failed refresh
   * keeps the last count, which something did measure.
   */
  const count = $derived(
    contexts.length > 0 || read === 'ready'
      ? `${contexts.length} context${contexts.length === 1 ? '' : 's'}`
      : read === 'loading'
        ? '…'
        : 'count unknown',
  );

  // A pointer landing outside the segment closes the list, the same one look
  // every other popover on the page takes.
  $effect(() => {
    if (!open) return;
    const away = (event: PointerEvent): void => {
      const at = event.target;
      if (at instanceof Node && segEl !== null && segEl.contains(at)) return;
      open = false;
    };
    document.addEventListener('pointerdown', away);
    return () => document.removeEventListener('pointerdown', away);
  });

  /** The toggle collapses, like every sibling row: touch has no hover, so this
   * is both its open and its close. */
  function toggle(): void {
    if (open) {
      if (leaving !== null) clearTimeout(leaving);
      leaving = null;
      open = false;
      return;
    }
    hold();
  }

  /** Escape closes from either of the list's controls, and focus goes back to
   * the toggle first - closing under the focused control would drop the focus
   * to the body, where Escape reaches nothing. */
  function esc(event: KeyboardEvent): void {
    if (event.key !== 'Escape') return;
    toToggle();
    if (leaving !== null) clearTimeout(leaving);
    leaving = null;
    open = false;
  }

  /** The person's close: saves, frees the name, and the row falls away. */
  function close(row: ContextRow): void {
    void closeContext(row.name).then(
      () => readContexts(),
      (error: unknown) => {
        // A close the shell refused leaves the context open, and the row says
        // so rather than vanishing over a name that is still held.
        read = 'failed';
        why = whyText(error);
      },
    );
  }
</script>

<span class="bz-seg" class:open bind:this={segEl}>
  <button
    type="button"
    class="bz-tog"
    aria-expanded={open}
    onclick={toggle}
    onpointerenter={(event) => {
      if (hovering(event)) hold();
    }}
    onpointerleave={(event) => {
      if (hovering(event)) release();
    }}
    onfocusin={focusIn}
    onfocusout={release}
    onkeydown={esc}
  >
    <Icon name="web" />
    browser
    <span class="n">{count}</span>
  </button>

  {#if open}
    <div
      class="bz-list"
      role="group"
      aria-label="the browser's contexts"
      onpointerenter={(event) => {
        if (hovering(event)) hold();
      }}
      onpointerleave={(event) => {
        if (hovering(event)) release();
      }}
      onfocusout={release}
    >
      <div class="bz-role">
        <span class="tx">
          {hosting ? 'browser connected' : 'browser not connected'}
        </span>
        {#if capable && !hosting}
          <button
            type="button"
            class="bz-take bz-takeover"
            aria-label="override the browser to this client"
            onclick={() => connection.takeBrowserRole()}
            onkeydown={esc}
          >
            override
          </button>
        {/if}
      </div>

      <!-- **The shared context is always there**, and it is the one most
           sessions drive - listing only named ones read as "no context"
           while a session was plainly using the browser. -->
      <div class="bz-it">
        {#if capable}
          <span class="ring"></span>
        {:else}
          <Icon name="x" class="bad" />
        {/if}
        <span class="nm">shared</span>
        <span class="tx">every session · the browser's own context</span>
      </div>

      {#each contexts as row (row.name)}
        <div class="bz-it">
          {#if row.running}
            <span class="ring"></span>
          {:else}
            <Icon name="x" class="bad" />
          {/if}
          <span class="nm">{row.name}</span>
          <span class="tx">{row.owner}{row.running ? '' : ' · its driver is gone'}</span>
          <button
            type="button"
            class="bz-take bz-close"
            aria-label="close the {row.name} context"
            onclick={() => close(row)}
            onkeydown={esc}
          >
            close
          </button>
        </div>
      {/each}
      {#if read === 'loading'}
        <div class="bz-it"><span class="tx">reading the contexts…</span></div>
      {:else if read === 'failed'}
        <div class="bz-it"><span class="tx bad">{why}</span></div>
      {:else if contexts.length === 0}
        <div class="bz-it"><span class="tx">no named contexts yet</span></div>
      {/if}
    </div>
  {/if}
</span>

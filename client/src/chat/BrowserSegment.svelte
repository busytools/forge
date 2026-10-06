<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import {
    canHost,
    closeContext,
    listContexts,
    showBrowser,
    whyText,
    type ContextRow,
  } from '../browser/host';
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
   * Three controls live here and each is the person's, never a session's:
   * **Take over** (the force override, where a click can honestly serve it),
   * **Show browser** (the visible toggle - a headed relaunch, whose cost the
   * list states), and a context row's **close** (which is what makes a
   * context whose owning session is gone recoverable).
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

  function toggle(): void {
    open = !open;
    if (open) void readContexts();
  }

  /** Escape closes from either of the list's controls. */
  function esc(event: KeyboardEvent): void {
    if (event.key === 'Escape') open = false;
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
  <button type="button" class="bz-tog" aria-expanded={open} onclick={toggle} onkeydown={esc}>
    <Icon name="web" />
    browser
    <span class="n">{count}</span>
  </button>

  {#if open}
    <div class="bz-list" role="group" aria-label="the browser's contexts">
      <div class="bz-role">
        <span class="tx">
          {hosting
            ? 'this client drives the browser'
            : capable
              ? 'another client drives the browser'
              : 'this client cannot drive the browser'}
        </span>
        {#if capable && !hosting}
          <button
            type="button"
            class="bz-take bz-takeover"
            onclick={() => connection.takeBrowserRole()}
            onkeydown={esc}
          >
            Take over
          </button>
        {/if}
      </div>

      {#if capable}
        <!-- The visible toggle: the app's own Chromium comes up as a window.
             A window is a launch flag, so this is a relaunch, and the cost
             rides the control where the decision is read. -->
        <div class="bz-it bz-window">
          <span class="nm">window</span>
          <button
            type="button"
            class="bz-take bz-show"
            onclick={() => void showBrowser()}
            onkeydown={esc}
          >
            Show browser
          </button>
        </div>
        <div class="bz-cost">
          Showing the browser restarts it: named contexts reopen from their saved cookies and tabs
          on their next call; the shared context's open tabs do not come back.
        </div>
      {/if}

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
        <div class="bz-it"><span class="tx">no contexts yet</span></div>
      {/if}
    </div>
  {/if}
</span>

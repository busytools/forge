<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { canHost, listContexts, type ContextRow } from '../browser/host';
  import type { Connection } from '../socket';

  /**
   * The browser row in the strip above the composer: how many named contexts
   * this client holds, and - opened - who owns each and which client drives
   * them.
   *
   * **A segment beside the agents row, not a page of its own.** The inspector
   * is going, and this is the shape its survivors take: a row in the strip,
   * its list opening in place (the mockup Ved settled, 2026-10-06). When the
   * surviving strip lands, this mounts into it unchanged.
   *
   * The contexts are the CLIENT's own state - it owns the drivers - so the
   * row reads them from the host it runs in, never from the server. The role
   * is the connection's fact, and Take over is offered only where a click can
   * honestly serve it: a page outside the shell holds no browser to take.
   */
  let { connection, capable = canHost() }: { connection: Connection; capable?: boolean } = $props();

  let open = $state(false);
  /** The strip's own snapshot: read at mount and when the list opens, not per frame. */
  let contexts = $state<ContextRow[]>([]);
  let hosting = $state(connection.browserRole());

  $effect(() => connection.onBrowserRole((now) => (hosting = now)));

  $effect(() => {
    void listContexts().then((rows) => (contexts = rows));
  });

  function toggle(): void {
    open = !open;
    if (open) void listContexts().then((rows) => (contexts = rows));
  }

  /** Escape closes from either control the list holds. */
  function esc(event: KeyboardEvent): void {
    if (event.key === 'Escape') open = false;
  }
</script>

<span class="bz-seg" class:open>
  <button type="button" class="bz-tog" aria-expanded={open} onclick={toggle} onkeydown={esc}>
    <Icon name="web" />
    browser
    <span class="n">{contexts.length} context{contexts.length === 1 ? '' : 's'}</span>
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
            class="bz-take"
            onclick={() => connection.takeBrowserRole()}
            onkeydown={esc}
          >
            Take over
          </button>
        {/if}
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
        </div>
      {/each}
      {#if contexts.length === 0}
        <div class="bz-it"><span class="tx">no contexts yet</span></div>
      {/if}
    </div>
  {/if}
</span>

<script lang="ts">
  /**
   * The Extensions page's route: the subject it watches, what the page
   * draws before the read lands, and the actions it dispatches.
   *
   * The subscription is this page's own and goes with it: only this page
   * draws the plugin inventory, and a client that has left it must not
   * leave forge scanning a plugin tree nobody sees.
   */
  import type { Command } from '../protocol';
  import type { Connection } from '../socket';
  import type { ProjectView } from '../wire/home';
  import { watchExtensions, type ExtensionsPage } from './live';
  import ExtensionsBody from './ExtensionsBody.svelte';

  let {
    connection,
    mark = null,
    projects = [],
  }: {
    connection: Connection | null;
    /** The brand mark from the greeting; unused by this page's plain lockup. */
    mark?: string | null;
    /** The roster, for the install scope picker's project list. */
    projects?: ProjectView[];
  } = $props();

  let page = $state<ExtensionsPage>({ read: null, refused: null });
  /**
   * The core's own words for a refused dispatch - a second action while one
   * runs, an unknown project - which arrives as an `error` frame on the
   * connection, so the page listens beside its own read.
   */
  let refusal = $state<string | null>(null);

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchExtensions(open).subscribe((value) => {
      page = value;
    });
  });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return open.onMessage((message) => {
      if (message.kind === 'error' && message.what === 'dispatch') refusal = message.why;
    });
  });

  /**
   * Dispatch one action.
   *
   * The outcome rides the subscription in every case - an action landing is
   * a `Plugins` frame - so nothing is asked for here; the page's own live
   * wiring turns the frame into a read. A dispatch with no open socket
   * throws before anything is sent, and that belongs in the same line as a
   * refusal.
   */
  function act(command: Command): void {
    const open = connection;
    if (open === null) return;
    refusal = null;
    try {
      void open.dispatch(command);
    } catch (why) {
      refusal = why instanceof Error ? why.message : String(why);
    }
  }

  function update(pluginId: string, scope: string): void {
    act({ plugin_update: { plugin_id: pluginId, scope } });
  }

  function updateall(): void {
    act('plugin_update_all');
  }

  function check(): void {
    act('plugin_check_updates');
  }

  function install(pluginId: string, scope: string, project: string | null): void {
    act({ plugin_install: { plugin_id: pluginId, scope, project } });
  }

  function repair(name: string): void {
    act({ marketplace_repair: { name } });
  }

  function addmarketplace(source: string): void {
    act({ marketplace_add: { source } });
  }
</script>

{#if page.refused !== null}
  <main class="wrap extensions">
    <!-- The server turned the subscription down, in its own words. The
         address is not what is wrong, so the door is not what is drawn. -->
    <p class="pending">This forge would not answer for its extensions: {page.refused}</p>
  </main>
{:else if page.read === null}
  <main class="wrap extensions"><p class="pending">Reading the extensions...</p></main>
{:else}
  <main class="wrap extensions">
    <ExtensionsBody
      wire={page.read}
      {refusal}
      {projects}
      {mark}
      {update}
      {updateall}
      {check}
      {install}
      {repair}
      {addmarketplace}
    />
  </main>
{/if}

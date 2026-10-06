<script lang="ts">
  import type { Command } from '../protocol';
  import type { Connection } from '../socket';
  import type { ModelRole } from '../wire/models';
  import { watchModels, type ModelsRead } from './live';
  import ModelsBody from './ModelsBody.svelte';

  /**
   * The models page's route: the subject it watches, what the page draws
   * before the read lands, and the actions it dispatches.
   *
   * The subscription is this page's own and goes with it: only this page
   * draws the catalogue, and a client that has left it must not leave forge
   * encoding a read nobody sees.
   */
  let { connection, mark = null }: { connection: Connection | null; mark?: string | null } =
    $props();

  let read = $state<ModelsRead>({ wire: null, refused: null });
  /**
   * The core's own words for a refused action - an install while one runs, an
   * activation on a role `forge.toml` pins - or `null`. It arrives as an
   * `error` frame on the connection, which is why the page listens beside its
   * own read.
   */
  let refusal = $state<string | null>(null);

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchModels(open).subscribe((value) => {
      read = value;
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
   * Dispatch one action and ask for the re-read straight after.
   *
   * The outcome rides the subscription in every case - a landed download or
   * activation is a `DictateModelsChanged` push - so the refresh is what
   * turns the page over to the core's own `downloading` / `loading` state
   * while the work is out. Without it a click draws nothing until it lands.
   * A dispatch with no open socket throws before anything is sent, and that
   * belongs in the same line as a refusal.
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
    open.refresh('dictate_models');
  }

  function check(): void {
    act('dictate_catalogue_check');
  }

  function install(variant: string): void {
    act({ dictate_install: { variant } });
  }

  function activate(file: string): void {
    act({ dictate_activate: { role: 'transcribing', file } });
  }

  function deactivate(role: ModelRole): void {
    act({ dictate_deactivate: { role } });
  }
</script>

{#if read.refused !== null}
  <main class="wrap models">
    <!-- The server turned the subscription down, in its own words. The
         address is not what is wrong, so the door is not what is drawn. -->
    <p class="pending">This forge would not answer for the models: {read.refused}</p>
  </main>
{:else if read.wire === null}
  <main class="wrap models"><p class="pending">Reading the models...</p></main>
{:else}
  <ModelsBody
    wire={read.wire}
    oncheck={check}
    oninstall={install}
    onactivate={activate}
    ondeactivate={deactivate}
    {refusal}
    {mark}
  />
{/if}

<script lang="ts">
  import type { Connection } from '../socket';
  import { watchModels, type ModelsRead } from './live';
  import ModelsBody from './ModelsBody.svelte';

  /**
   * The models page's route: the subject it watches, and what the page draws
   * before the read lands.
   *
   * The subscription is this page's own and goes with it: only this page
   * draws the catalogue, and a client that has left it must not leave forge
   * encoding a read nobody sees.
   */
  let { connection, mark = null }: { connection: Connection | null; mark?: string | null } =
    $props();

  let read = $state<ModelsRead>({ wire: null, refused: null });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return watchModels(open).subscribe((value) => {
      read = value;
    });
  });

  /**
   * Check the catalogue now.
   *
   * The dispatch's outcome rides the subscription, and a check takes as long
   * as the feed's own listing does - so the read is asked for straight after,
   * which is what turns the line over to the core's `checking` state while the
   * fetch is out. Without it a click draws nothing until the check lands.
   */
  function check(): void {
    const open = connection;
    if (open === null) return;
    void open.dispatch('dictate_catalogue_check');
    open.refresh('dictate_models');
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
  <ModelsBody wire={read.wire} oncheck={check} {mark} />
{/if}

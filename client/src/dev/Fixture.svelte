<script lang="ts">
  import { DEFAULT_ADDRESS } from '../connect/attempt';
  import Home from '../home/Home.svelte';
  import ChatFixture from './ChatFixture.svelte';
  import { loadFixtureHome } from './fixture';

  /**
   * The home drawn from the server's own fixture, reached only in a
   * development build and only from the router's `/fixture` route.
   *
   * It exists so a page can be looked at without a running forge. It is not a
   * fallback: `loadFixtureHome` answers `null` in a production build, so
   * nothing ships that could be drawn in a server's absence.
   */
  const loaded = loadFixtureHome();
</script>

{#await loaded then wire}
  {#if wire}
    <!-- The default address, so the band's web card reads as it does behind a
         connection rather than blank. -->
    <Home {wire} address={DEFAULT_ADDRESS} />
  {/if}
{/await}

<!-- The conversation column under it, so the chat can be looked at without a
     running forge. It is a harness rather than a page: nothing ships it. -->
<ChatFixture />

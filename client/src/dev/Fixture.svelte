<script lang="ts">
  import Home from '../home/Home.svelte';
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
    <Home {wire} />
  {/if}
{/await}

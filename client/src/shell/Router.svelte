<script lang="ts">
  import type { Attempt } from '../connect/attempt';
  import Connect from '../connect/Connect.svelte';
  import Fixture from '../dev/Fixture.svelte';
  import Home from '../home/Home.svelte';
  import type { HomeRead } from '../home/live';
  import type { Route } from '../routes';
  import type { ClientSettings } from '../wire/types';

  let {
    route,
    settings,
    address,
    home,
    connected,
    onconnect,
  }: {
    route: Route;
    settings: ClientSettings;
    address: string;
    home: HomeRead;
    /** A socket is open, whether or not its first read has come back. */
    connected: boolean;
    onconnect: (connected: Extract<Attempt, { ok: true }>) => void;
  } = $props();
</script>

{#if route.name === 'connect'}
  <Connect {settings} {onconnect} />
{:else if route.name === 'home'}
  {#if home.wire}
    <Home wire={home.wire} {address} mark={settings.mark} />
  {:else if home.refused}
    <!-- The server turned the subscription down, in its own words. Handing
         the door back here would say the address was wrong, which is the one
         thing it is not. -->
    <main class="wrap">
      <p class="pending">This forge would not answer for the home: {home.refused}</p>
    </main>
  {:else if connected}
    <!-- Connected, and the first read has not come back. It is its own
         state: handing the door back here re-renders a fresh form with the
         address reset, and a second Enter would open a second socket. -->
    <main class="wrap"><p class="pending">Reading the fleet...</p></main>
  {:else}
    <!-- No server has answered, and the app's only input is its URL: the
         connect screen stays rather than a page falling back to bundled
         data. -->
    <Connect {settings} {onconnect} />
  {/if}
{:else if route.name === 'fixture' && import.meta.env.DEV}
  <!-- Behind the same guard as the loader: the connect screen stays the front
       door in every build, and the route renders nothing without it. -->
  <Fixture />
{:else if route.name === 'session'}
  <!-- Task 5 draws the session page here, from `src/session/`. -->
  <main class="wrap"><p class="pending">The session page is next.</p></main>
{:else}
  <main class="wrap">
    <p class="pending">
      That is not a page forge serves. The home is at <a href="/">/</a>, and a session at
      <code>/session/&lt;org&gt;/&lt;project&gt;/&lt;label&gt;</code>.
    </p>
  </main>
{/if}

<style>
  .pending {
    color: var(--muted);
    font-size: var(--fs-base);
    margin-top: 12vh;
    text-align: center;
  }

  a {
    color: var(--accent);
  }

  code {
    font-family: var(--mono);
    color: var(--text);
  }
</style>

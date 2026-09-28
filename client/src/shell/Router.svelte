<script lang="ts">
  import Connect from '../connect/Connect.svelte';
  import Fixture from '../dev/Fixture.svelte';
  import Home from '../home/Home.svelte';
  import type { Route } from '../routes';
  import type { HomeWire } from '../wire/home';
  import type { ClientSettings } from '../wire/types';

  let {
    route,
    settings,
    address,
    wire,
    onconnect,
  }: {
    route: Route;
    settings: ClientSettings;
    address: string;
    wire: HomeWire | null;
    onconnect: (connected: {
      url: string;
      settings: ClientSettings;
      wire: HomeWire | null;
    }) => void;
  } = $props();
</script>

{#if route.name === 'connect'}
  <Connect {settings} {onconnect} />
{:else if route.name === 'home'}
  {#if wire}
    <Home {wire} {address} mark={settings.mark} />
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
  <div class="wrap"><p class="pending">The session page is next.</p></div>
{:else}
  <div class="wrap">
    <p class="pending">
      That is not a page forge serves. The home is at <a href="/">/</a>, and a session at
      <code>/session/&lt;org&gt;/&lt;project&gt;/&lt;label&gt;</code>.
    </p>
  </div>
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

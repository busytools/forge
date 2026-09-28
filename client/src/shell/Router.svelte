<script lang="ts">
  import Connect from '../connect/Connect.svelte';
  import type { Route } from '../routes';
  import type { ClientSettings } from '../wire/types';

  let {
    route,
    settings,
    address,
    onconnect,
  }: {
    route: Route;
    settings: ClientSettings;
    address: string;
    onconnect: (connected: { url: string; settings: ClientSettings }) => void;
  } = $props();
</script>

{#if route.name === 'connect'}
  <Connect {settings} {onconnect} />
{:else if route.name === 'home'}
  <!-- Task 4 draws the home here, from `src/home/`. -->
  <div class="wrap"><p class="pending">The home is next.</p></div>
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

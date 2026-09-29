<script lang="ts">
  import type { Attempt } from '../connect/attempt';
  import Composer from '../composer/Composer.svelte';
  import { dictationOffered } from '../composer/view';
  import Connect from '../connect/Connect.svelte';
  import Fixture from '../dev/Fixture.svelte';
  import Home from '../home/Home.svelte';
  import type { HomeRead } from '../home/live';
  import type { Route } from '../routes';
  import Session from '../session/Session.svelte';
  import type { Connection } from '../socket';
  import type { ClientSettings } from '../wire/types';

  let {
    route,
    settings,
    address,
    home,
    failure,
    connected,
    connection,
    onconnect,
  }: {
    route: Route;
    settings: ClientSettings;
    address: string;
    home: HomeRead;
    /** Why the launch did not open the home, for the door to draw. */
    failure: Extract<Attempt, { ok: false }> | null;
    /** A socket is open, whether or not its first read has come back. */
    connected: boolean;
    /** The connection the pages read through, which a session page subscribes on. */
    connection: Connection | null;
    onconnect: (connected: Extract<Attempt, { ok: true }>) => void;
  } = $props();

  /**
   * Whether this install can dictate, which is the home's read rather than the
   * seat's: the engine is process-wide, and the page's own wire carries it.
   */
  const dictate = $derived(home.wire === null ? false : dictationOffered(home.wire.dictate));
</script>

{#if route.name === 'connect'}
  <Connect {settings} initialAddress={address} launchFailure={failure} {onconnect} />
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
    <Connect {settings} initialAddress={address} launchFailure={failure} {onconnect} />
  {/if}
{:else if route.name === 'fixture' && import.meta.env.DEV}
  <!-- Behind the same guard as the loader: the connect screen stays the front
       door in every build, and the route renders nothing without it. -->
  <Fixture />
{:else if route.name === 'session'}
  {#if connection !== null && home.wire !== null}
    <!-- The box is wired in HERE and nowhere else, and the page takes two things
         from its presence: the composer it draws, and whether this client can
         answer the prompts that composer shows. Absent it, a session page has
         no box at all and the seat is subscribed as an observer. -->
    <Session slot={route.slot} {connection} wire={home.wire}>
      {#snippet composer(props)}
        <Composer {...props} dictation={dictate} />
      {/snippet}
    </Session>
  {:else if connected}
    <!-- Connected, and the fleet has not been read yet. The session page's
         rail and four of its inspector sections are the home's, so handing it
         one that has not arrived would draw them empty rather than pending. -->
    <main class="wrap"><p class="pending">Reading the fleet...</p></main>
  {:else}
    <!-- No server has answered, so there is no seat to draw and the app's
         only input is its URL. -->
    <Connect {settings} initialAddress={address} launchFailure={failure} {onconnect} />
  {/if}
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

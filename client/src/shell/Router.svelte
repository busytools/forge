<script lang="ts">
  import Chat from '../chat/Chat.svelte';
  import type { Attempt } from '../connect/attempt';
  import Composer from '../composer/Composer.svelte';
  import { dictationOffered } from '../composer/view';
  import Connect from '../connect/Connect.svelte';
  import Fixture from '../dev/Fixture.svelte';
  import Board from '../board/Board.svelte';
  import Home from '../home/Home.svelte';
  import type { HomeRead } from '../home/live';
  import Models from '../models/Models.svelte';
  import { titleFor, type Route } from '../routes';
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
    notice,
    sessionNotice = null,
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
    /**
     * The protocol-skew line the door draws, already worded: the shell hands
     * it down because only the shell watches a skew appear and clear, and
     * the shell stands down on this route so this is the door's own copy.
     * `null` when there is no skew.
     */
    notice: string | null;
    /**
     * The same line for a session page, which draws it in its rail footer:
     * the shell's strip is in-flow, and a session page is `100dvh`, so the
     * strip there is a page that scrolls. `null` on every other route.
     */
    sessionNotice?: string | null;
    onconnect: (connected: Extract<Attempt, { ok: true }>) => void;
  } = $props();

  /**
   * Whether this install can dictate, which is the home's read rather than the
   * seat's: the engine is process-wide, and the page's own wire carries it.
   */
  const dictate = $derived(home.wire === null ? false : dictationOffered(home.wire.dictate));

  /**
   * The board's edits, sent over the socket as commands: the user's own
   * moves, app-level (no seat routes them), each carrying its project.
   * Without a connection the page's controls draw and do nothing.
   */
  function boardAct(command: Record<string, Record<string, unknown>>): void {
    void connection?.dispatch(command);
  }

  // The tab's name follows what is on screen: forge at the home, the seat's
  // project on a session, and its label too when it is a worker's.
  $effect(() => {
    document.title = titleFor(route);
  });
</script>

{#if route.name === 'connect'}
  <Connect {settings} initialAddress={address} launchFailure={failure} {notice} {onconnect} />
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
    <Connect {settings} initialAddress={address} launchFailure={failure} {notice} {onconnect} />
  {/if}
{:else if route.name === 'board'}
  {#if home.wire !== null}
    <!-- One project's board, a takeover over wherever the reader was: the
         top bar's back and done return through history. The page reads the
         same snapshot the fleet does - its rows ride the wire. -->
    <Board wire={home.wire} org={route.org} project={route.project} onact={boardAct} />
  {:else if connected}
    <main class="wrap"><p class="pending">Reading the fleet...</p></main>
  {:else}
    <Connect {settings} initialAddress={address} launchFailure={failure} {notice} {onconnect} />
  {/if}
{:else if route.name === 'fixture' && import.meta.env.DEV}
  <!-- Behind the same guard as the loader: the connect screen stays the front
       door in every build, and the route renders nothing without it. -->
  <Fixture />
{:else if route.name === 'session'}
  {#if connection !== null && home.wire !== null}
    <!-- Both columns the session page draws are handed over HERE, and this is
         the only place that does it. The page takes them as snippets rather
         than importing them, so one it is not given is a column that draws
         nothing at all: no loading row, no empty copy, no way to tell a
         missing column from a quiet one. It is also where the composer's
         presence decides whether this client can answer the prompts it
         shows; absent it, the seat is subscribed as an observer. -->
    <Session
      slot={route.slot}
      {connection}
      wire={home.wire}
      mark={settings.mark}
      notice={sessionNotice}
    >
      {#snippet conversation(props)}
        <Chat {...props} />
      {/snippet}
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
    <Connect {settings} initialAddress={address} launchFailure={failure} {notice} {onconnect} />
  {/if}
{:else if route.name === 'models'}
  {#if connection !== null}
    <!-- The page subscribes the catalogue itself and reads the connection
         directly: its own state (loading, refused, the four sections) is the
         page's, and nothing outside it draws any of it. -->
    <Models {connection} mark={settings.mark} />
  {:else}
    <Connect {settings} initialAddress={address} launchFailure={failure} {notice} {onconnect} />
  {/if}
{:else}
  <main class="wrap">
    <p class="pending">
      That is not a page forge serves. The home is at <a href="/">/</a>, a session at
      <code>/session/&lt;org&gt;/&lt;project&gt;/&lt;label&gt;</code>, and the models at
      <code>/models</code>.
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

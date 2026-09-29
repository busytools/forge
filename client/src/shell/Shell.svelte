<script lang="ts">
  import { onMount } from 'svelte';

  import Sprite from '../components/Sprite.svelte';
  import { DEFAULT_ADDRESS, displayAddress, type Attempt } from '../connect/attempt';
  import { boot } from '../connect/boot';
  import { rememberedAddress } from '../connect/remembered';
  import { watchHome, type HomeRead } from '../home/live';
  import { hrefFor, parseRoute, type Route } from '../routes';
  import type { Connection, ConnectionStatus } from '../socket';
  import { applySettings } from '../theme';
  import { DEFAULT_SETTINGS, type ClientSettings } from '../wire/types';
  import Router from './Router.svelte';

  // The connect screen is the front door: it is the first thing a person
  // meets, and a deep link is the one reason to open anywhere else. So `/`
  // opens the door rather than the home, and the URL is moved with it so a
  // reload lands in the same place.
  const opened = parseRoute(location.pathname);
  const onDoor = opened.name === 'home';
  // Read once, at launch. The address the app opens on is also the one the
  // door's field carries, whether or not there was one to open on.
  const remembered = rememberedAddress();

  let route = $state<Route>(onDoor ? { name: 'connect' } : opened);
  let settings = $state<ClientSettings>(DEFAULT_SETTINGS);
  let address = $state(remembered ?? DEFAULT_ADDRESS);
  let home = $state<HomeRead>({ wire: null, refused: null });
  // Raw, so the connection is handed around as the object it is rather than
  // as a reactive proxy of it.
  let connection = $state.raw<Connection | null>(null);
  let connectionStatus = $state<ConnectionStatus>('connecting');
  let failure = $state<Extract<Attempt, { ok: false }> | null>(null);
  // A remembered address is a claim that something answered there once, so it
  // is tried before there is a page to draw - a door drawn over an attempt in
  // flight offers a second Connect over a socket already opening.
  let booting = $state(onDoor && remembered !== null);

  onMount(() => {
    if (remembered === null) {
      // Nothing to open on, so the door, at the URL that names it.
      if (onDoor) history.replaceState(null, '', hrefFor({ name: 'connect' }));
      return;
    }
    void open(remembered);
  });

  /**
   * Open on the remembered address: the home when something answers there,
   * and the door carrying the reason when nothing does.
   *
   * No `go` on the way to the home, because the launch is already addressed
   * at `/` and pushing it would put a second entry of the same page behind
   * the reader.
   */
  async function open(remembered: string) {
    const launched = await boot(remembered);
    booting = false;
    failure = launched.failure;
    if (launched.connected) take(launched.connected);
    route = launched.route;
  }

  $effect(() => {
    applySettings(settings, document.documentElement);
  });

  $effect(() => {
    const restore = () => {
      route = parseRoute(location.pathname);
    };
    addEventListener('popstate', restore);
    return () => removeEventListener('popstate', restore);
  });

  /**
   * The home's subscription, which lives as long as the shell does: the home
   * is the page behind every other.
   *
   * The wire survives a drop. What does not survive it is the claim that the
   * page is current, so the status is watched in the same effect: without it
   * a page keeps drawing pre-drop data with nothing saying so, for as long as
   * the backoff takes and every retry after it.
   */
  $effect(() => {
    const open = connection;
    if (open === null) return;
    connectionStatus = open.status();

    const stopHome = watchHome(open).subscribe(($home) => {
      home = $home;
    });
    const stopStatus = open.onStatus((next) => {
      connectionStatus = next;
    });
    return () => {
      stopHome();
      stopStatus();
    };
  });

  /** The connection is open, so what the page draws is being kept current. */
  const live = $derived(connectionStatus === 'open');

  function go(next: Route) {
    route = next;
    history.pushState(null, '', hrefFor(next));
  }

  /** The connection the pages read through, and what it came with. */
  function take(connected: Extract<Attempt, { ok: true }>) {
    // A second connect would otherwise leave the first socket open, still
    // subscribed to the home and still re-reading it, for the rest of the
    // session - and nothing would be drawing what it was keeping current.
    connection?.close();
    settings = connected.settings;
    address = connected.url;
    connection = connected.connection;
  }

  function connect(connected: Extract<Attempt, { ok: true }>) {
    take(connected);
    go({ name: 'home' });
  }

  /**
   * Follow a link in place.
   *
   * Every page draws real `<a href>` elements, so a link works with the
   * keyboard, opens in a new tab on a modifier, and reads as a link to
   * assistive tech. This keeps an unmodified click inside the app instead of
   * reloading it, which is the one thing a real anchor does that an SPA does
   * not want.
   */
  function follow(event: MouseEvent) {
    if (event.defaultPrevented || event.button !== 0) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const target = event.target;
    if (!(target instanceof Element)) return;
    const anchor = target.closest('a[href]');
    if (!anchor || anchor.getAttribute('target') === '_blank') return;
    const href = anchor.getAttribute('href');
    if (!href || !href.startsWith('/')) return;
    event.preventDefault();
    go(parseRoute(href));
  }
</script>

<!--
  The listener goes on the body rather than on a wrapper element, because the
  sheet already owns `.app`: it is the session page's three-column grid, and a
  same-named wrapper here would hand the shell the session's columns.
-->
<svelte:body onclick={follow} />

<!-- Once per page: a `<use>` reference resolves against the document it is in. -->
<Sprite />

{#if booting}
  <!-- A landmark, so the one thing on screen sits inside one like every page. -->
  <main class="wrap">
    <p class="opening">Opening {displayAddress(address)}...</p>
  </main>
{:else}
  {#if connectionStatus === 'mismatched'}
    <p class="stale" role="alert">
      That forge speaks a protocol this client does not. The two halves have to match, so one of
      them needs updating.
    </p>
  {:else if connection !== null && !live}
    <!-- A live region rather than a landmark: the pages below each carry the
         page's own `main`, and a second one would be a second page. -->
    <p class="stale" role="status">
      Reconnecting to {displayAddress(address)} - showing the last read
    </p>
  {/if}

  <Router
    {route}
    {settings}
    {address}
    {home}
    {failure}
    connected={connection !== null}
    onconnect={connect}
  />
{/if}

<style>
  .opening {
    color: var(--muted);
    font-size: var(--fs-base);
    margin-top: 12vh;
    text-align: center;
  }

  .stale {
    background: var(--warn-bg, transparent);
    color: var(--muted);
    font-size: var(--fs-label);
    padding: 6px 12px;
    text-align: center;
  }
</style>

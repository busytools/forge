<script lang="ts">
  import Sprite from '../components/Sprite.svelte';
  import { displayAddress, type Attempt } from '../connect/attempt';
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

  let route = $state<Route>(onDoor ? { name: 'connect' } : opened);
  let settings = $state<ClientSettings>(DEFAULT_SETTINGS);
  let address = $state('');
  let home = $state<HomeRead>({ wire: null, refused: null });
  // Raw, so the connection is handed around as the object it is rather than
  // as a reactive proxy of it.
  let connection = $state.raw<Connection | null>(null);
  let connectionStatus = $state<ConnectionStatus>('connecting');

  if (onDoor) history.replaceState(null, '', hrefFor({ name: 'connect' }));

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

  function connect(connected: Extract<Attempt, { ok: true }>) {
    // A second connect would otherwise leave the first socket open, still
    // subscribed to the home and still re-reading it, for the rest of the
    // session - and nothing would be drawing what it was keeping current.
    connection?.close();
    settings = connected.settings;
    address = connected.url;
    connection = connected.connection;
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

{#if connectionStatus === 'mismatched'}
  <p class="stale" role="alert">
    That forge speaks a protocol this client does not. The two halves have to match, so one of them
    needs updating.
  </p>
{:else if connection !== null && !live}
  <!-- A live region rather than a landmark: the pages below each carry the
       page's own `main`, and a second one would be a second page. -->
  <p class="stale" role="status">
    Reconnecting to {displayAddress(address)} - showing the last read
  </p>
{/if}

<Router {route} {settings} {address} {home} connected={connection !== null} onconnect={connect} />

<style>
  .stale {
    background: var(--warn-bg, transparent);
    color: var(--muted);
    font-size: var(--fs-label);
    padding: 6px 12px;
    text-align: center;
  }
</style>

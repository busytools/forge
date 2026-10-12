<script lang="ts">
  import { onMount, untrack } from 'svelte';

  import Sprite from '../components/Sprite.svelte';
  import { DEFAULT_ADDRESS, displayAddress, type Attempt } from '../connect/attempt';
  import { boot } from '../connect/boot';
  import { rememberedAddress } from '../connect/remembered';
  import { watchHome, type HomeRead } from '../home/live';
  import { skewMessage, subjectKey, type Skew } from '../protocol';
  import { hrefFor, parseRoute, type Route } from '../routes';
  import { forgetClosed, removedLanding, watchReleases, watchRemovals } from '../session/close';
  import type { Connection, ConnectionStatus } from '../socket';
  import { applySettings } from '../theme';
  import { watchUpdate } from '../update/state';
  import { DEFAULT_SETTINGS, type ClientSettings, type SessionSlot } from '../wire/types';
  import Router from './Router.svelte';

  // The connect screen is the front door: it is the first thing a person
  // meets, and a deep link is the one reason to open anywhere else. So the
  // root is what launches - it opens the door when there is nothing to open on
  // - and the URL is moved to `/connect` so a reload lands in the same place.
  const opened = parseRoute(location.pathname);
  const onDoor = opened.name === 'home';
  // Read once, at launch. The address the app opens on is also the one the
  // door's field carries, whether or not there was one to open on.
  const remembered = rememberedAddress();

  let route = $state<Route>(onDoor ? { name: 'connect' } : opened);
  let settings = $state<ClientSettings>(DEFAULT_SETTINGS);
  let address = $state(remembered ?? DEFAULT_ADDRESS);
  let home = $state<HomeRead>({ wire: null, refused: null, report: null });
  // Raw, so the connection is handed around as the object it is rather than
  // as a reactive proxy of it.
  let connection = $state.raw<Connection | null>(null);
  let connectionStatus = $state<ConnectionStatus>('connecting');
  /**
   * What the live connection's greeting said this client's protocol does not
   * agree with, or `null`. Set from the connection rather than from the
   * attempt that took it, because a page left open across a forge upgrade
   * meets the skew again on the reconnect.
   */
  let skew = $state<Skew | null>(null);
  let failure = $state<Extract<Attempt, { ok: false }> | null>(null);
  // A remembered address is a claim that something answered there once, so it
  // is tried before there is a page to draw - a door drawn over an attempt in
  // flight offers a second Connect over a socket already opening.
  let booting = $state(onDoor && remembered !== null);
  /**
   * Whether a read has landed since the connection last opened.
   *
   * A reopen re-asks and the server encodes the home by reading each
   * project's working tree, so there is a window after `onopen` where the
   * page is drawing pre-drop rows. Clearing the notice on `open` would hide
   * exactly that window, which is the one place a reader cannot tell the
   * difference between stale and current.
   */
  let settled = $state(false);

  onMount(() => {
    // Nothing to open on, so the door, at the URL that names it.
    if (onDoor && remembered === null) {
      history.replaceState(null, '', hrefFor({ name: 'connect' }));
    }
    void open();
    // Once per launch, whatever route it opened on - the notice draws wherever
    // the home does, and a check the reader has to revisit the home for is a
    // check that is not made.
    void watchUpdate();
  });

  /**
   * Open on the remembered address: the home when something answers there,
   * and the door carrying the reason when nothing does.
   *
   * No `go` on the way to the home, because a launch from the root is already
   * addressed at `/` and pushing it would put a second entry of the same page
   * behind the reader.
   */
  async function open() {
    // What a connection taken while this is in flight is measured against.
    const held = connection;
    const launched = await boot(opened, remembered);
    booting = false;
    if (connection !== held) {
      // A submit is a person acting on what is in front of them and a launch is
      // the app guessing, so theirs is the one to keep. The launch's socket
      // goes with its result: nothing would ever draw from it.
      launched.connected?.connection.close();
      return;
    }
    failure = launched.failure;
    if (launched.connected) take(launched.connected);
    // `null` is a route the app was addressed at. A launch opens the socket
    // its page reads but leaves the page itself alone.
    if (launched.route !== null) route = launched.route;
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
    skew = open.skew();

    const stopHome = watchHome(open).subscribe(($home) => {
      home = $home;
      if ($home.wire !== null) {
        settled = true;
        // The roster catching up is a home read like any other: a closed
        // seat that has landed (asleep, or gone) drops its mark the moment
        // the wire says so, which is what takes the row's "going to sleep"
        // away. Waiting on a removal frame instead left the words up until
        // a reload for every close that lands by sleeping (#1712).
        forgetClosed($home.wire);
      }
    });
    const stopStatus = open.onStatus((next) => {
      connectionStatus = next;
      if (next !== 'open') settled = false;
    });
    // A greeting lands after the socket opens and changes no status, so the
    // status alone would never announce a skew - and a reconnect after a
    // forge upgrade is exactly where one appears or clears.
    const stopGreetings = open.onMessage((message) => {
      if (message.kind === 'greeting') skew = open.skew();
    });
    return () => {
      stopHome();
      stopStatus();
      stopGreetings();
    };
  });

  /**
   * A seat taken out from under the reader moves them off it.
   *
   * Two frames ask for it: the removal a cascade or a despawn lands
   * (`WorkerStatusChanged`), and the release announcement every close begins
   * with - the only word a viewer gets when the close was made in ANOTHER
   * view (#1930). The reader's own close lands them in `closeSeat`, and the
   * marks are forgotten as the roster catches up, so a project started again
   * is not suppressed by an old one.
   */
  $effect(() => {
    const open = connection;
    if (open === null) return;
    const land = (seat: SessionSlot, spawnedBy: SessionSlot | null): void => {
      // Both reads are untracked, so the effect subscribes once per
      // connection rather than once per roster read and route change.
      const wire = untrack(() => home.wire);
      if (wire === null) return;
      forgetClosed(wire);
      const showing = untrack(() => route);
      const onSeat =
        showing.name === 'session' &&
        subjectKey({ session: showing.slot }) === subjectKey({ session: seat });
      if (onSeat) go(removedLanding(wire, seat, spawnedBy, Date.now()));
    };
    const stopRemovals = watchRemovals(open, land);
    const stopReleases = watchReleases(open, (seat) => land(seat, null));
    return () => {
      stopRemovals();
      stopReleases();
    };
  });

  /**
   * The page has a read behind it and is no longer current.
   *
   * Both halves are needed: without the read the notice would sit above
   * "Reading the fleet..." claiming a last read that does not exist, and
   * without the staleness it would show while everything is fine.
   */
  const stale = $derived(
    connection !== null && home.wire !== null && !(connectionStatus === 'open' && settled),
  );

  /** What it says, which depends on whether anything is still trying. */
  const staleLine = $derived(
    connectionStatus === 'closed'
      ? `The connection to ${displayAddress(address)} was closed - showing the last read`
      : `Reconnecting to ${displayAddress(address)} - showing the last read`,
  );

  /**
   * The one line a protocol skew draws here, or `null` when this surface
   * draws none.
   *
   * The notice stands down on the door, which draws its own copy in its own
   * column rather than having a second identical strip stacked above it.
   */
  const skewLine = $derived(skew === null || route.name === 'connect' ? null : skewMessage(skew));

  /**
   * The same notice, for a session page: it draws the line in its rail
   * footer - where the build facts live - rather than take the strip. The
   * strip is an in-flow row, and an in-flow row above a page declared
   * `100dvh` is a page that scrolls: the stray window scrollbar Ved saw over
   * the chat's own. The door's copy stands down the same way.
   */
  const sessionNotice = $derived(
    route.name !== 'session' ? null : (skewLine ?? (stale ? staleLine : null)),
  );

  /** The notice the door draws for itself. */
  const doorNotice = $derived(skew === null ? null : skewMessage(skew));

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
    // As the person wrote it rather than the socket URL: this is what a field
    // shows them if they come back to the door.
    address = connected.address;
    connection = connected.connection;
    // The reason belonged to the launch that produced it. Left standing, the
    // door a Back lands on accuses a forge that has just answered.
    failure = null;
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
  {#if skewLine && sessionNotice === null}
    <!-- A status, because the page behind it is live: the notice reads the
         two stamps, it does not stop anything. -->
    <p class="stale" role="status">{skewLine}</p>
  {:else if stale && sessionNotice === null}
    <!-- A live region rather than a landmark: the pages below each carry the
         page's own `main`, and a second one would be a second page. -->
    <p class="stale" role="status">{staleLine}</p>
  {/if}

  <Router
    {route}
    {settings}
    {address}
    {home}
    {failure}
    {connection}
    notice={doorNotice}
    {sessionNotice}
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

  /* `--s2` rather than a wash of its own: the token set is closed, and a
     notice is a raised surface. */
  .stale {
    background: var(--s2);
    color: var(--text);
    font-size: var(--fs-label);
    padding: 6px 12px;
    text-align: center;
  }
</style>

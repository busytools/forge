<script lang="ts">
  import { hrefFor, parseRoute, type Route } from '../routes';
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

  function go(next: Route) {
    route = next;
    history.pushState(null, '', hrefFor(next));
  }

  function connect(connected: { url: string; settings: ClientSettings }) {
    settings = connected.settings;
    address = connected.url;
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

<Router {route} {settings} {address} onconnect={connect} />

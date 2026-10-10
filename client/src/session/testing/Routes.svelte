<script lang="ts">
  import type { ComponentProps } from 'svelte';
  import type { Readable } from 'svelte/store';

  import type { Route } from '../../routes';
  import Router from '../../shell/Router.svelte';

  /**
   * The shell's own mount, with the route a test can move.
   *
   * **A seat switch is the route becoming a different one**, and the route a
   * test mounts with cannot change - so it arrives as a store, the way the
   * shell hands the Router a route it re-derives on every popstate.
   */
  let {
    route,
    props,
  }: { route: Readable<Route>; props: Omit<ComponentProps<typeof Router>, 'route'> } = $props();
</script>

<Router route={$route} {...props} />

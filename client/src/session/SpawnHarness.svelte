<script lang="ts">
  import type { Connection } from '../socket';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import Session from './Session.svelte';

  /**
   * A session page whose home snapshot a test can move, which is how `Spawning`
   * arriving is driven: the roster gaining the seat's row is what says the
   * project is up.
   */
  let { slot, connection, home }: { slot: SessionSlot; connection: Connection; home: HomeWire } =
    $props();

  // The prop is the starting value and the test moves it from there, which is
  // the one read that is not a re-render.
  // svelte-ignore state_referenced_locally
  export const page: { home: HomeWire } = $state({ home });
</script>

<Session {slot} {connection} wire={page.home} />

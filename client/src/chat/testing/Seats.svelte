<script lang="ts">
  import type { Readable } from 'svelte/store';

  import type { Connection } from '../../socket';
  import type { SessionSlot } from '../../wire/types';
  import Chat from '../Chat.svelte';

  /**
   * A column a test can move between seats.
   *
   * **A seat change is a prop becoming a different one**, and a prop a test
   * mounts with cannot change - so the seat arrives as a store, the way the
   * shell hands the page a slot it re-derives.
   */
  let { seat, connection }: { seat: Readable<SessionSlot>; connection: Connection } = $props();
</script>

<Chat slot={$seat} {connection} />

<script lang="ts">
  import Chat from '../Chat.svelte';
  import type { Connection } from '../../socket';
  import type { SessionSlot } from '../../wire/types';

  /**
   * The column with a seat it can be moved to.
   *
   * **The app changes the slot on a mounted column**, and that is what the
   * switch tests need: re-mounting clears what the keep exists to hold, so a
   * redraw would test the teardown rather than the switch.
   */
  let {
    connection,
    first,
    second,
  }: { connection: Connection; first: SessionSlot; second: SessionSlot } = $props();

  let forward = $state(true);

  /** Move to the other seat, the way a switch does - and back on the next call. */
  export function flip(): void {
    forward = !forward;
  }

  const slot = $derived(forward ? first : second);
</script>

<Chat {connection} {slot} />

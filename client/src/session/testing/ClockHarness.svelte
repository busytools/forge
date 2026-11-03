<script lang="ts">
  import Chat from '../../chat/Chat.svelte';
  import Session from '../Session.svelte';
  import type { BoardRead } from '../../wire/board';
  import type { HomeWire } from '../../wire/home';
  import type { Connection } from '../../socket';
  import type { SessionSlot } from '../../wire/types';

  /** Scratch harness: the real conversation column inside the seat page, so a
   *  tick's reach into it can be measured. */
  let {
    slot,
    connection,
    wire,
    board,
  }: {
    slot: SessionSlot;
    connection: Connection;
    wire: HomeWire;
    board: BoardRead;
  } = $props();
</script>

<Session {slot} {connection} {wire} {board}>
  {#snippet conversation(props)}
    <Chat {...props} />
  {/snippet}
</Session>

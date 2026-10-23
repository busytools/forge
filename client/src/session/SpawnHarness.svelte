<script lang="ts">
  import type { Connection } from '../socket';
  import type { BoardRead } from '../wire/board';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import Session from './Session.svelte';

  /**
   * A session page whose home and board snapshots a test can move, which is
   * how `Spawning` arriving is driven: the roster gaining the seat's row is
   * what says the project is up, and a board read moving a task is what the
   * tasks row follows (#2517).
   */
  let {
    slot,
    connection,
    home,
    board = { wire: null, refused: null },
  }: {
    slot: SessionSlot;
    connection: Connection;
    home: HomeWire;
    board?: BoardRead;
  } = $props();

  // The props are the starting values and the test moves them from there,
  // which is the one read that is not a re-render.
  // svelte-ignore state_referenced_locally
  export const page: { home: HomeWire; board: BoardRead } = $state({ home, board });
</script>

<Session {slot} {connection} wire={page.home} board={page.board} />

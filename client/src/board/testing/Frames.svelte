<script lang="ts">
  import type { BoardRead, BoardWire } from '../../wire/board';
  import type { HomeWire } from '../../wire/home';
  import Board from '../Board.svelte';

  /**
   * The board over a wire the test drives: `set` swaps in the next answer,
   * which is what a server's reply looks like to the page's own fold - a
   * case a test needs when the fold's decision depends on what the wire
   * holds after an edit.
   */
  let {
    initial,
    onact = null,
    mode = 'board',
  }: {
    initial: { home: HomeWire; board: BoardWire };
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
    /** Which of the board's three views the frame opens on. */
    mode?: 'window' | 'board' | 'archived';
  } = $props();

  // svelte-ignore state_referenced_locally
  let wire = $state(initial.home);
  // svelte-ignore state_referenced_locally
  let board = $state<BoardRead>({ wire: initial.board, refused: null });

  export function set(next: { home: HomeWire; board: BoardWire }): void {
    wire = next.home;
    board = { wire: next.board, refused: null };
  }
</script>

<!-- The page over a wire the test drives: the mode picks which view's
     interactions the frame exercises. -->
<Board {wire} {board} org="TestOrg" project="proj" {onact} initialMode={mode} />

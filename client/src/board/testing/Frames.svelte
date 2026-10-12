<script lang="ts">
  import type { HomeWire } from '../../wire/home';
  import Board from '../Board.svelte';

  /**
   * The board over a wire the test drives: `set` swaps in the next snapshot,
   * which is what a server's answer looks like to the page's own fold - a
   * case a test needs when the fold's decision depends on what the wire
   * holds after an edit.
   */
  let {
    initial,
    onact = null,
  }: {
    initial: HomeWire;
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
  } = $props();

  // svelte-ignore state_referenced_locally
  let wire = $state(initial);

  export function set(next: HomeWire): void {
    wire = next;
  }
</script>

<Board {wire} org="TestOrg" project="proj" {onact} />

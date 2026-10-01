<script lang="ts">
  /**
   * A popover a test can move, which is what the composer does and `mount`
   * alone cannot: a key moves the mark, and typing rebuilds the list under it,
   * while the component underneath is the same one.
   *
   * Test support rather than a page: nothing the app ships imports this file.
   */
  import Autocomplete from './Autocomplete.svelte';
  import type { Offer } from './autocomplete';

  let { offer: first, onpick }: { offer: Offer; onpick: (at: number) => void } = $props();

  /** Which row a key would take, which the test moves as a key does. */
  let marked = $state(0);
  /** The list being offered, which the test replaces as a query does. */
  // The initial list is the point: a test moves this rather than re-rendering
  // the harness into being, which would lose what it had already done.
  // svelte-ignore state_referenced_locally
  let shown = $state(first);

  /**
   * The mark and the list, handed out so a test moves either one the way the
   * composer does.
   *
   * Not named `marked` or `offer`: an export that shadows the state the
   * template reads leaves the template reading a value that never moves.
   */
  export const page = {
    get at() {
      return marked;
    },
    set at(next: number) {
      marked = next;
    },
    set offer(next: Offer) {
      shown = next;
    },
  };
</script>

<Autocomplete offer={shown} {marked} {onpick} />

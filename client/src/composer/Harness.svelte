<script lang="ts">
  /**
   * A page that can hand the composer a new record, which is what a test needs
   * and `mount` alone cannot do.
   *
   * **The props live here in `$state`, and that is the point.** The composer
   * reads its props reactively and holds the reader's draft in its own state,
   * so a test that mounted it again for a second record would be testing a
   * fresh component with a fresh draft - which is precisely the defect the
   * draft tests exist to catch. `set` is what a page re-render is.
   *
   * Test support rather than a page: nothing the app ships imports this file.
   */
  import type { SessionSlot } from '../wire/types';
  import type { ComposerProps, ComposerRecord, SeatRead } from './view';
  import { props as start, type Wire } from './testing';
  import Composer from './Composer.svelte';

  let {
    wire,
    initial = {},
    dictation = false,
  }: {
    wire: Wire;
    /** Anything the test wants different about the first render. */
    initial?: Partial<ComposerProps>;
    dictation?: boolean;
  } = $props();

  // The initial values are the point: this is a page standing still, and a test
  // moves it by setting a field rather than by re-rendering it into being.
  // svelte-ignore state_referenced_locally
  let held = $state<ComposerProps>({
    ...start(initial),
    connection: wire.connection,
    dictation,
  });

  /**
   * The props this page is holding, handed out so a test can set one the way
   * the page would have: `harness.page.record = ...` is a re-render.
   *
   * Not named `props`: the compiler's own `$props()` binding owns that name
   * inside a component, and an export that shadows it is read before it exists.
   */
  export const page = {
    get record() {
      return held.record;
    },
    set record(next: ComposerRecord) {
      held = { ...held, record: next };
    },
    get seat() {
      return held.seat;
    },
    set seat(next: SeatRead) {
      held = { ...held, seat: next };
    },
    /**
     * The seat the page is showing, which moves with the record when a reader
     * leaves one seat and lands on another.
     */
    get slot() {
      return held.slot;
    },
    set slot(next: SessionSlot) {
      held = { ...held, slot: next };
    },
  };
</script>

<Composer {...held} />

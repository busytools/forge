<script lang="ts">
  /**
   * The pile and the box in one container, which is how the page composes
   * them - `Session.svelte` renders the queue and the composer inside one
   * `.composer` div, and the walk's handback reaches the box through it.
   *
   * **A test harness, because `mount` alone cannot hand a new `rows`.** The
   * walk's two handbacks are about the pile CHANGING under the reader - the
   * row it points at leaving, the whole pile emptying - and both only happen
   * on a re-render with new props. `page.rows` is what a re-render is.
   */
  import type { Connection } from '../../socket';
  import type { QueuedPromptRow } from '../../session/wire';
  import type { SessionSlot } from '../../wire/types';
  import Queue from '../Queue.svelte';

  let {
    rows,
    slot,
    connection,
  }: {
    rows: QueuedPromptRow[];
    slot: SessionSlot;
    connection: Connection;
  } = $props();

  // The initial value is the point: this is a page standing still, and a test
  // moves it by setting `page.rows` rather than by mounting it again.
  // svelte-ignore state_referenced_locally
  let held = $state<QueuedPromptRow[]>(rows);

  /** What a test re-renders with: `harness.page.rows = []` is the pile emptying. */
  export const page = {
    get rows() {
      return held;
    },
    set rows(next: QueuedPromptRow[]) {
      held = next;
    },
  };
</script>

<div class="composer">
  <Queue rows={held} {slot} {connection} />
  <!-- The composer's own editor name, so the census reads it as the box it imitates. -->
  <textarea data-editor="composer"></textarea>
</div>

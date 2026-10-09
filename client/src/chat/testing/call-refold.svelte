<script lang="ts">
  import { untrack } from 'svelte';

  import Call from '../Call.svelte';
  import type { ToolLeaf } from '../leaves';

  /**
   * The call row with a call a test can re-hand, the way the fold does.
   *
   * The fold builds the leaf fresh on every frame of a streaming turn, so
   * the row receives a NEW object with the same values; a test that mounts
   * `Call` directly can never show that. `refold` is that replacement, and
   * `settle` moves the call from a run in flight to one that has come back,
   * which is the transition that turns a held row into a readable one.
   */
  let { call, onreadoutput }: { call: ToolLeaf; onreadoutput: (id: string) => void } = $props();

  let held = $state(untrack(() => call));

  /** Re-hand the call, value-identical - the fold's own shape. */
  export function refold(): void {
    held = { ...held };
  }

  /** The call comes back: the run in flight is over. */
  export function settle(): void {
    held = { ...held, status: 'completed' };
  }
</script>

<Call call={held} k="f1" open={true} {onreadoutput} />

<script lang="ts">
  import { onMount } from 'svelte';

  import type { Connection } from '../../socket';
  import type { TurnInfo } from '../units';
  import Pinned from '../Pinned.svelte';

  /**
   * The pinned row with frames that keep landing.
   *
   * A frame is what the row re-folds on, and the row's clock is a timer armed
   * in an effect - so a frame landing faster than the timer's interval is
   * exactly the case where a timer that restarts per frame never fires
   * (`Strip.svelte`'s comment carries the reasoning). The swap is a fresh
   * object every 400 ms, which is what a changed read looks like to the effect.
   *
   * The fixture is the record `Pinned.test.ts` draws too - a running turn with
   * no usage yet, stamped where its clock stands still - and it lives here
   * rather than arriving as a prop, because the row below reads the seeded
   * value and a prop read at init is only ever its first value anyway.
   */
  let { connection }: { connection: Connection } = $props();

  let info = $state<TurnInfo>({
    running: true,
    failed: false,
    duration_ms: 0,
    api_ms: null,
    ended_at_utc: '2026-10-01T06:00:00Z',
    model: 'claude-opus-5',
    thinking_tokens: null,
    input_tokens: null,
    output_tokens: null,
    cache_read_tokens: null,
    cache_written_tokens: null,
    session_cost_usd: null,
  });

  onMount(() => {
    const id = setInterval(() => {
      info = { ...info, duration_ms: (info.duration_ms ?? 0) + 1 };
    }, 400);
    return () => clearInterval(id);
  });
</script>

<Pinned {info} {connection} />

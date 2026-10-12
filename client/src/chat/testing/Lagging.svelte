<script lang="ts">
  import type { Connection } from '../../socket';
  import type { TurnInfo } from '../units';
  import Pinned from '../Pinned.svelte';

  /**
   * A pinned row whose record slips BEHIND its clock, the way a late-arriving
   * correction can: `duration_ms` and the stamp move together the wrong way -
   * the span only half a second while the stamp sits three seconds in - and
   * the local tick has already carried the display past both.
   *
   * The record is `Pinned.test.ts`'s running fixture, so the clock starts at
   * the same instant and only the slip is under test.
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

  /** The correction that arrives behind the clock. */
  export function lag(): void {
    info = { ...info, duration_ms: 500, ended_at_utc: '2026-10-01T06:00:03Z' };
  }
</script>

<Pinned {info} {connection} />

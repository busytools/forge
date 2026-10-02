<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { clock, duration, money, tokens } from './numbers';
  import { attributed, cached, elapsed } from './report';
  import Strip from './Strip.svelte';
  import type { TurnInfo } from './units';

  /**
   * What a settled turn did, under the work it did it with: its wall clock, its
   * API time, and the tokens and cost the CLI reported.
   *
   * **The row itself is `Strip`'s**, which the box above the composer draws as
   * well: the two are one row in two places, and its rules are one copy.
   *
   * **A field the record does not carry is held with a dash in the body and
   * dropped from the row.** Neither ever writes a zero for an absent value:
   * the CLI attributing nothing arrives as a zero block, and a zero here reads
   * as a measurement.
   */
  let { info }: { info: TurnInfo } = $props();

  /** The record with an unattributed usage block dropped, which is the rule the terminal applies. */
  const held = $derived(attributed(info));

  /** The turn's own clock, read from the instant it ended in the reader's zone. */
  const ended = $derived(clock(held.ended_at_utc));

  /**
   * What the turn spent on its own tools and hooks: the span less the API's
   * share.
   *
   * `0s` is a measurement - a turn whose whole span was API time - and only a
   * record missing either end draws the dash. A check that demanded the span
   * be the LARGER of the two would drop that zero, which the terminal's own
   * `checked_sub` keeps.
   */
  const local = $derived(
    held.duration_ms !== null && held.api_ms !== null && held.duration_ms >= held.api_ms
      ? held.duration_ms - held.api_ms
      : null,
  );

  /** The share of this turn's input the cache served, by `report.ts`'s rule. */
  const share = $derived(cached(held));

  /** What the body draws: a label and its figure, one fact per pair. */
  const facts = $derived.by(() => {
    const dash = '-';
    const pairs: Array<{ label: string; value: string }> = [
      // A running turn's last stamp is the last frame it drew, not an end, so
      // the row does not claim one.
      { label: 'ended', value: held.running ? dash : (ended ?? dash) },
      { label: 'model', value: held.model ?? dash },
      { label: 'elapsed', value: elapsed(held, Date.now()) },
      { label: 'api', value: held.api_ms === null ? dash : duration(held.api_ms) },
      { label: 'local', value: local === null ? dash : `${duration(local)} tools + hooks` },
      {
        label: 'thinking',
        value: held.thinking_tokens === null ? dash : `${tokens(held.thinking_tokens)} est`,
      },
      { label: 'in', value: held.input_tokens === null ? dash : tokens(held.input_tokens) },
      { label: 'out', value: held.output_tokens === null ? dash : tokens(held.output_tokens) },
      {
        label: 'cache',
        value: held.cache_read_tokens === null ? dash : `${tokens(held.cache_read_tokens)} read`,
      },
      {
        label: 'wrote',
        value: held.cache_written_tokens === null ? dash : tokens(held.cache_written_tokens),
      },
      {
        label: 'session',
        value: held.session_cost_usd === null ? dash : `${money(held.session_cost_usd)} cumulative`,
      },
    ];
    if (share !== null) pairs.push({ label: 'cached', value: `${share}% of input` });
    return pairs;
  });
</script>

<details class="turninfo">
  <summary>
    <Strip info={held} />
    <Chevron />
  </summary>
  <!-- Each fact is a pair of its own, placed where it is: the design kept a
       cell in column with an empty span beside it, which is a grid auto-flowing
       rather than a body saying what it holds. -->
  <div class="tibody">
    {#each facts as fact (fact.label)}
      <span class="fact">
        <b>{fact.label}</b>
        <span>{fact.value}</span>
      </span>
    {/each}
  </div>
</details>

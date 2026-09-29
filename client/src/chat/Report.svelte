<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { clock, duration, money, tokens } from './numbers';
  import type { TurnInfo } from './units';

  /**
   * What a settled turn did, under the work it did it with: its wall clock, its
   * API time, and the tokens and cost the CLI reported.
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

  /**
   * The share of this turn's input served from the cache, over every
   * input-side counter.
   *
   * `null` when the record carries no cache read at all, which is a turn that
   * never touched the cache rather than one that missed it entirely.
   */
  const cached = $derived.by(() => {
    const read = held.cache_read_tokens;
    if (read === null) return null;
    const total = read + (held.input_tokens ?? 0) + (held.cache_written_tokens ?? 0);
    return total === 0 ? null : Math.floor((read * 100) / total);
  });

  /** What the body draws: a label and its figure, one fact per pair. */
  const facts = $derived.by(() => {
    const dash = '-';
    const pairs: Array<{ label: string; value: string }> = [
      { label: 'ended', value: ended ?? dash },
      { label: 'model', value: held.model ?? dash },
      { label: 'elapsed', value: duration(held.duration_ms) },
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
    if (cached !== null) pairs.push({ label: 'cached', value: `${cached}% of input` });
    return pairs;
  });

  /**
   * The record with an unattributed usage block dropped.
   *
   * A frame whose counters are all zero is the CLI saying it has nothing to
   * attribute, and a compaction result is that shape; a real zero inside a
   * block that does carry counters is a measurement, and prints as one.
   */
  function attributed(info: TurnInfo): TurnInfo {
    const nothing =
      (info.input_tokens ?? 0) === 0 &&
      (info.output_tokens ?? 0) === 0 &&
      (info.cache_read_tokens ?? 0) === 0 &&
      (info.cache_written_tokens ?? 0) === 0;
    return nothing
      ? {
          ...info,
          input_tokens: null,
          output_tokens: null,
          cache_read_tokens: null,
          cache_written_tokens: null,
        }
      : info;
  }
</script>

<details class="turninfo">
  <summary>
    <!-- The settled mark, from the sprite rather than from a character cell:
         the arrow this used to be was drawn because a terminal had nothing
         else, and it reads as punctuation beside real icons. -->
    <Icon name="check" class="st" />
    <span>{duration(held.duration_ms)}</span>
    {#if held.input_tokens !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span
        >{tokens(held.input_tokens)}{'\u{2191}'}{#if held.output_tokens !== null}
          {tokens(held.output_tokens)}{'\u{2193}'}{/if}</span
      >
    {/if}
    {#if cached !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span>{cached}% cached</span>
    {/if}
    {#if held.cache_written_tokens !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span>{tokens(held.cache_written_tokens)} written</span>
    {/if}
    {#if held.session_cost_usd !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span>{money(held.session_cost_usd)} cumulative</span>
    {/if}
    <span class="tog"></span>
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

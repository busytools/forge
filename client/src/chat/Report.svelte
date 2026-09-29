<script lang="ts">
  import type { TurnInfo } from './units';
  import { clock, duration, money, tokens } from './numbers';

  /**
   * What a settled turn did, under the work it did it with: its wall clock, its
   * API time, and the tokens and cost the CLI reported.
   *
   * **A field the record does not carry is dropped from the row and held with a
   * dash in the body.** Neither ever writes a zero for an absent value: the CLI
   * attributing nothing arrives as a zero block, and a zero here reads as a
   * measurement.
   */
  let { info, live = false }: { info: TurnInfo; live?: boolean } = $props();

  /** The record with an unattributed usage block dropped, which is the rule the terminal applies. */
  const held = $derived(attributed(info));

  /** What the body draws: a label and its figure, one fact per pair. */
  const facts = $derived.by(() => {
    const dash = '-';
    const pairs: Array<{ label: string; value: string; wide?: boolean }> = [
      { label: 'ended', value: clock(held.ended_at_utc) ?? dash },
      { label: 'model', value: held.model ?? dash },
      { label: 'elapsed', value: duration(held.duration_ms) },
      { label: 'api', value: held.api_ms === null ? dash : duration(held.api_ms) },
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
    {#if live}
      <span class="st"><span class="ring"></span></span>
    {:else}
      <!-- A settled turn's mark, from the sprite rather than from a character
           cell: the arrow this used to be was drawn because a terminal had
           nothing else, and it reads as punctuation beside real icons. -->
      <svg class="ic st"><use href="#i-check"></use></svg>
    {/if}
    <span>{duration(held.duration_ms)}</span>
    {#if live && held.thinking_tokens !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span>thinking {tokens(held.thinking_tokens)}</span>
    {/if}
    {#if held.input_tokens !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span
        >{tokens(held.input_tokens)}{'\u{2191}'}{#if held.output_tokens !== null}
          {tokens(held.output_tokens)}{'\u{2193}'}{/if}</span
      >
    {/if}
    {#if held.cache_written_tokens !== null}
      <span class="sep">{'\u{b7}'}</span>
      <span>{tokens(held.cache_written_tokens)} written</span>
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

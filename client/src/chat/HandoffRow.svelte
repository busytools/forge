<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import { grouped } from './numbers';
  import { shortId, type HandoffRecord, type HandoffSection } from '../wire/handoff';

  /**
   * The handoff row: where this seat's occupant was replaced by a fresh
   * session that continued from a brief.
   *
   * **It is a hint, like the compaction point, and it holds more.** The
   * closed row says which two sessions met and how full the context was; the
   * open one holds the two halves of the brief the successor was given - the
   * ground truth forge read off the transcript and git, and the judgment the
   * handing-over session wrote. Nothing is dropped: the previous
   * conversation stays above the row and the old session is one `/resume`
   * away.
   */
  let {
    handoff,
    open = false,
  }: {
    handoff: HandoffRecord;
    open?: boolean;
  } = $props();

  let opened = $state(untrack(() => open));

  const pair = $derived(`${shortId(handoff.from)} \u{2192} ${shortId(handoff.to)}`);

  /** Why the swap happened, in the row's own words. A value this build does
   * not know draws as itself rather than as nothing. */
  const why = $derived(reasonText(handoff));

  /** The line under the open row: the fill it fired at, when it had one. */
  const meta = $derived.by(() => {
    const parts: string[] = [];
    if (handoff.reason !== 'fill') parts.push(why);
    if (handoff.fill_percent !== null) {
      parts.push(`${handoff.fill_percent}% of the window`);
    }
    if (handoff.total_tokens !== null && handoff.max_tokens !== null) {
      parts.push(`${grouped(handoff.total_tokens)} of ${grouped(handoff.max_tokens)} read`);
    }
    parts.push('turn end');
    return parts.join(' \u{b7} ');
  });

  /** The chip a closed row wears: the fill for a fill trigger, the reason
   * for any other - a compaction backstop must not read as a percentage. */
  const chip = $derived(
    handoff.reason === 'fill' && handoff.fill_percent !== null ? `${handoff.fill_percent}%` : why,
  );

  function reasonText(record: HandoffRecord): string {
    switch (record.reason) {
      case 'fill':
        return `${record.at_percent}% of the window`;
      case 'compaction':
        return 'a compaction came first';
      case 'manual':
        return 'asked by hand';
      default:
        return record.reason;
    }
  }

  function lines(section: HandoffSection): string {
    return section.lines.join('\n');
  }
</script>

<details class="handoff" bind:open={opened}>
  <summary>
    <span class="rule"></span>
    <span class="word">handoff</span>
    <span class="pair">{pair}</span>
    <span class="pct">{chip}</span>
    <Chevron />
    <span class="rule"></span>
  </summary>
  {#if opened}
    <div class="hbody">
      <div class="meta">{meta}</div>
      <div class="lbl">PART 1 &#183; GROUND TRUTH</div>
      {#each handoff.ground_truth as section, at (section.label + ':' + at)}
        <div class="fact">
          <span class="k">{section.label}</span>
          <span class="v">{lines(section)}</span>
        </div>
      {/each}
      {#if handoff.judgment.length > 0}
        <div class="lbl">PART 2 &#183; JUDGMENT</div>
        {#each handoff.judgment as section, at (section.label + ':' + at)}
          <div class="fact">
            <span class="k">{section.label}</span>
            <span class="v">{lines(section)}</span>
          </div>
        {/each}
      {/if}
    </div>
  {/if}
</details>

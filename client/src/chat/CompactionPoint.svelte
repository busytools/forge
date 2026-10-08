<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { grouped, tokens } from './numbers';
  import Prose from './Prose.svelte';

  /**
   * The compaction point: where the conversation was cut and the transcript
   * replaced. It is the settled record of the compaction whose in-flight form
   * the composer draws, and it sits in the conversation at the boundary
   * rather than in the header - the header's count says how many, this says
   * where.
   *
   * A hint rather than a block: a hairline across the column carrying the word,
   * the count read before the cut, and a handle onto the trigger behind it.
   *
   * **It says what it was given.** Each of the three facts a boundary's frame
   * carries - the trigger, the count read before the cut, the count carried
   * after it - draws only when the frame held one. A boundary whose metadata
   * did not survive the wire at all keeps the hairline and drops the handle
   * rather than opening onto nothing, and one that kept some of its facts opens
   * onto those alone.
   *
   * **The continuation prompt the CLI sends after a cut opens under the
   * facts**, so the compaction stays readable - the summary is what the model
   * was handed, and it is the one account of what was dropped.
   */
  let {
    trigger,
    preTokens,
    postTokens,
    summary,
    open = false,
  }: {
    trigger: string | null;
    preTokens: number | null;
    postTokens: number | null;
    summary: string | null;
    open?: boolean;
  } = $props();

  /** Whether the point is open; a closed one carries its summary and nothing else. */
  let opened = $state(open);

  /** Whether anything sits behind the row, which is what the handle promises. */
  const opens = $derived(
    trigger !== null || preTokens !== null || postTokens !== null || summary !== null,
  );

  /** Whether the frame carried any of the three facts the first line draws. */
  const hasFacts = $derived(trigger !== null || preTokens !== null || postTokens !== null);
</script>

<details class="cpoint" bind:open={opened}>
  <summary>
    <span class="rule"></span>
    <span class="word">compaction</span>
    {#if preTokens !== null}<span class="n">{tokens(preTokens)} before</span>{/if}
    {#if opens}<Chevron />{/if}
    <span class="rule"></span>
  </summary>
  {#if opened && opens}
    <div class="cpbody">
      {#if hasFacts}
        {#if trigger !== null}trigger <b>{trigger}</b
          >{#if preTokens === null && postTokens !== null},{/if}{/if}
        {#if trigger !== null && preTokens !== null}{' \u{b7} '}{/if}
        {#if preTokens !== null}<b>{grouped(preTokens)}</b> tokens read before the cut{#if postTokens !== null},{/if}{/if}
        {#if postTokens !== null}<b>{grouped(postTokens)}</b> carried after it{/if}
      {/if}
      {#if summary !== null}
        <div class="sum"><Prose text={summary} /></div>
      {/if}
    </div>
  {/if}
</details>

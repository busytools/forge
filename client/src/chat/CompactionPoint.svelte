<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import { grouped, tokens } from './numbers';

  /**
   * The compaction point: where the conversation was cut and the transcript
   * replaced. It is what the in-flight `Compacting context...` line settles
   * into, and it sits in the conversation at the boundary rather than in the
   * header - the header's count says how many, this says where.
   *
   * A hint rather than a block: a hairline across the column carrying the word,
   * the count read before the cut, and a handle onto the trigger behind it.
   *
   * **It says what it was given.** The CLI's frame also carries the count after
   * the cut, which forge's decode drops before any view sees it, so the body
   * states the trigger and the pre-cut count and stops. A boundary whose
   * metadata did not survive the wire at all keeps the hairline and drops the
   * handle rather than opening onto nothing, and one that kept a single fact
   * opens onto that fact alone.
   */
  let { trigger, preTokens }: { trigger: string | null; preTokens: number | null } = $props();

  /** Whether anything sits behind the row, which is what the handle promises. */
  const opens = $derived(trigger !== null || preTokens !== null);
</script>

<details class="cpoint">
  <summary>
    <span class="rule"></span>
    <span class="word">compaction</span>
    {#if preTokens !== null}<span class="n">{tokens(preTokens)} before</span>{/if}
    {#if opens}<Chevron />{/if}
    <span class="rule"></span>
  </summary>
  {#if opens}
    <div class="cpbody">
      {#if trigger !== null}trigger <b>{trigger}</b>{/if}
      {#if trigger !== null && preTokens !== null}{' \u{b7} '}{/if}
      {#if preTokens !== null}<b>{grouped(preTokens)}</b> tokens read before the cut{/if}
    </div>
  {/if}
</details>

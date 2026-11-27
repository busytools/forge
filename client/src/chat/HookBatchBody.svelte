<script lang="ts">
  import { duration } from './numbers';
  import type { HookBatchLeaf } from './units';

  /**
   * What one stop-hook batch ran, as every surface that draws a batch draws
   * it: each hook's command and duration, the errors the CLI reported against
   * the batch (which name no single hook), and any context the batch added to
   * the session.
   *
   * **One body for the two places a batch draws** - the carrying batch's own
   * row and the turn's chip - so the two cannot drift on what a batch
   * contains.
   */
  let { batch }: { batch: HookBatchLeaf } = $props();

  /**
   * How long one hook took, which is milliseconds where the row has them.
   *
   * Every hook in the captured corpus ran in under a second (3ms, 6ms, 412ms),
   * and the row's own drawing says `3ms`: the shared span formatter floors
   * those to `0.0s`, which is a duration drawn as no duration at all.
   */
  function took(ms: number): string {
    return ms < 1000 ? `${ms}ms` : duration(ms);
  }
</script>

{#each batch.infos as info, at (at)}
  <div class="term">
    {info.command}{#if info.durationMs !== undefined}{' \u{b7} '}{took(info.durationMs)}{/if}
  </div>
{/each}
{#each batch.errors as error, at (at)}
  <div class="term"><span class="fail">{error}</span></div>
{/each}
{#each batch.additionalContext as line, at (at)}
  <div class="term"><span class="sum">{line}</span></div>
{/each}
<!-- The two facts a batch can carry with no line of their own: output the
     summary names no hook for, and a hook that held the stop back. Either one
     is why the batch is a row rather than a count in the turn's chip. -->
{#if batch.hasOutput && batch.infos.length === 0 && batch.additionalContext.length === 0 && batch.errors.length === 0}
  <div class="term">a hook printed output the CLI named no hook for</div>
{/if}
{#if batch.preventedContinuation}
  <div class="term"><span class="sum">a hook kept the turn going</span></div>
{/if}

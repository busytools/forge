<script lang="ts">
  /**
   * **The one narrative hold.** Every state that takes the composer's box -
   * a wake, a command in flight, a compaction, a handoff - draws here, with
   * the same sweep and only the words changing. The error state draws in the
   * same band in its own tone, because it is not progress.
   *
   * The states themselves are decided by `blocked()`; this draws what it is
   * handed, so a line added there is a line drawn here.
   */
  let {
    line,
    sub = null,
    bad = false,
    shimmer = false,
  }: { line: string; sub?: string | null; bad?: boolean; shimmer?: boolean } = $props();
</script>

<div class="blocked" class:err={bad}>
  <span class="b1"><span class:shimmer>{line}</span></span>
  {#if sub !== null}<span class="b2">{sub}</span>{/if}
</div>

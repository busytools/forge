<script lang="ts">
  import type { Notice } from './units';

  /**
   * A line nobody typed: an external delivery, a scheduled fire, or a failure
   * the workspace reported.
   *
   * The severity is the word the row leads with rather than a colour alone,
   * because a reader who cannot see the ground still has to know whether this
   * is a delivery or a failure.
   */
  let { notice }: { notice: Notice } = $props();

  const tone = $derived(
    notice.severity === 'warning' ? 'warn' : notice.severity === 'error' ? 'err' : 'info',
  );
  const word = $derived(
    notice.severity === 'warning' ? 'Warning' : notice.severity === 'error' ? 'Error' : 'Info',
  );
</script>

<div class="notice {tone}">
  {#if notice.chip !== undefined}
    <span class="chip">{notice.chip}</span>
  {/if}
  <span class="sev">{word}</span>
  {notice.text}
  {#if notice.sub !== undefined}
    <span class="sub2">{notice.sub}</span>
  {/if}
</div>

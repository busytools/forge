<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import type { Notice } from './units';

  /**
   * A line nobody typed: an external delivery, a scheduled fire, or a failure
   * the workspace reported.
   *
   * The severity is the word the row leads with rather than a colour alone,
   * because a reader who cannot see the ground still has to know whether this
   * is a delivery or a failure.
   *
   * **A provisional notice opens onto its payload.** A frame this build has
   * no face for yet draws as itself, marked with its own type, and the whole
   * frame sits behind the open - the Hook row's shape, so the decision about
   * its real face is made from what arrived rather than from a description of
   * it (Ved, 2026-10-26).
   */
  let { notice }: { notice: Notice } = $props();

  const tone = $derived(
    notice.severity === 'warning' ? 'warn' : notice.severity === 'error' ? 'err' : 'info',
  );
  const word = $derived(
    notice.severity === 'warning' ? 'Warning' : notice.severity === 'error' ? 'Error' : 'Info',
  );
</script>

{#snippet head()}
  {#if notice.mark !== undefined}
    <span class="mark">{notice.mark}</span>
  {/if}
  <span class="sev">{word}</span>
  {notice.text}
{/snippet}

{#if notice.raw === undefined}
  <div class="notice {tone}">
    {#if notice.chip !== undefined}
      <span class="chip">{notice.chip}</span>
    {/if}
    {@render head()}
    {#if notice.sub !== undefined}
      <span class="sub2">{notice.sub}</span>
    {/if}
  </div>
{:else}
  <details class="notice {tone}">
    <summary>
      {#if notice.chip !== undefined}
        <span class="chip">{notice.chip}</span>
      {/if}
      {@render head()}
      {#if notice.sub !== undefined}
        <span class="sub2">{notice.sub}</span>
      {/if}
      <Chevron />
    </summary>
    <div class="rawbox"><pre>{notice.raw}</pre></div>
  </details>
{/if}

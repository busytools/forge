<script lang="ts">
  /**
   * The retraction, wherever a comment's card is drawn: one `delete` act
   * and, because a delete cannot be undone, an inline confirm in its place -
   * the act asks, the two answers decide. One component for every surface,
   * so the words and the shape are the same on all of them.
   */
  let {
    ondelete,
    busy = false,
  }: {
    /** Remove the thread this control belongs to. */
    ondelete: () => void | Promise<void>;
    /** The page's write in flight, which holds the act with the others. */
    busy?: boolean;
  } = $props();

  let asking = $state(false);
  let taking = $state(false);

  async function take(): Promise<void> {
    taking = true;
    await ondelete();
    taking = false;
    asking = false;
  }
</script>

{#if asking}
  <span class="rv-delask">
    <span class="rv-delq">delete this comment?</span>
    <button class="rv-a danger" type="button" disabled={taking || busy} onclick={() => void take()}
      >delete</button
    >
    <button class="rv-a" type="button" onclick={() => (asking = false)}>keep</button>
  </span>
{:else}
  <button
    class="rv-a"
    type="button"
    disabled={busy}
    aria-label="delete this comment"
    onclick={() => (asking = true)}>delete</button
  >
{/if}

<style>
  .rv-delask {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .rv-delq {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--warn);
  }
</style>

<script lang="ts">
  /**
   * The takeover: the client's screen replaced by the browser, with the bar
   * that gets you back.
   *
   * **The stage under the bar belongs to the shell's engine** - the Mac's CEF
   * embed and Android's WebView fill it natively - so the web side draws
   * nothing there. The bar, the way back and Done are the web side's.
   */
  import { takeover } from './takeover.svelte';

  let { address = '' }: { address?: string } = $props();
</script>

<svelte:window
  onkeydown={(event: KeyboardEvent) => {
    if (event.key === 'Escape' && takeover.active) void takeover.back();
  }}
/>

<div class="takeover">
  <div class="bar">
    <button type="button" class="back" onclick={() => void takeover.back()}>
      <span class="arw">←</span> back to forge
    </button>
    <span class="addr">{address}</span>
    {#if takeover.asking !== null}
      <button type="button" class="done" onclick={() => void takeover.done()}>Done</button>
    {/if}
  </div>
  <!-- The engine's area: covered natively, empty here on purpose. -->
  <div class="stage" aria-hidden="true"></div>
</div>

<script lang="ts">
  /**
   * The browser takeover's page area: the whole screen under the dock's bar.
   *
   * **The bar is the dock's** - it carries the hand-off's actions and the
   * feedback line, with the same words row, take card and mic as every other
   * box. This component is the picture and the states before a frame lands.
   */
  import { onMount } from 'svelte';

  import { setRegion, surface } from './surface.svelte';

  let pageEl: HTMLDivElement;

  function measure(): void {
    if (!pageEl) return;
    const box = pageEl.getBoundingClientRect();
    setRegion(
      { x: box.x, y: box.y, width: box.width, height: box.height },
      window.devicePixelRatio || 1,
    );
  }

  onMount(() => {
    measure();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    observer?.observe(pageEl);
    window.addEventListener('resize', measure);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', measure);
    };
  });

  /**
   * **The takeover IS full screen.** The shell takes the window to its simple
   * full screen when the surface opens and gives it back when the surface
   * closes (the store's phase). The app's green button is not the door.
   */
  async function fullscreen(on: boolean): Promise<void> {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      await invoke('browser_takeover', { on });
    } catch (why) {
      console.warn('the takeover could not change full screen', why);
    }
  }

  onMount(() => {
    void fullscreen(true);
    return () => void fullscreen(false);
  });

  $effect(() => {
    if (surface.phase === 'closed') void fullscreen(false);
  });
</script>

<div class="takeover" data-phase={surface.phase} style:top={`${surface.barHeight}px`}>
  <div class="takeover-page" bind:this={pageEl} role="region" aria-label="the app's browser">
    {#if surface.phase === 'opening'}
      <p class="surface-tx">opening the browser…</p>
    {:else if surface.phase === 'failed'}
      <p class="surface-tx bad">{surface.reason}</p>
    {/if}
  </div>
</div>

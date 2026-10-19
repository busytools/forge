<script lang="ts">
  /**
   * The conversation column's browser surface: the measuring stick and the
   * state line.
   *
   * While the surface is live the native layer is drawn over this element,
   * exactly at its box - the page under it is not what the person sees. The
   * states before a frame lands, or when the shell refused, are drawn here.
   */
  import { onMount } from 'svelte';

  import { closeSurface, setRegion, surface } from './surface.svelte';

  let el: HTMLDivElement;

  function measure(): void {
    if (!el) return;
    const box = el.getBoundingClientRect();
    setRegion(
      { x: box.x, y: box.y, width: box.width, height: box.height },
      window.devicePixelRatio || 1,
    );
  }

  onMount(() => {
    measure();
    // jsdom has no ResizeObserver; a WebView that lacks it still measures on
    // mount and on the window's own resize.
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    observer?.observe(el);
    window.addEventListener('resize', measure);
    return () => {
      observer?.disconnect();
      window.removeEventListener('resize', measure);
    };
  });
</script>

<div
  class="surface"
  class:live={surface.phase === 'live'}
  bind:this={el}
  role="region"
  aria-label="the app's browser"
  data-phase={surface.phase}
>
  {#if surface.phase === 'opening'}
    <p class="surface-tx">opening the browser…</p>
  {:else if surface.phase === 'failed'}
    <p class="surface-tx bad">{surface.reason}</p>
    <button class="btn" type="button" onclick={() => void closeSurface()}>close</button>
  {/if}
</div>

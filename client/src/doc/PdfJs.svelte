<script lang="ts">
  /**
   * A PDF drawn by pdf.js, for the one surface with no viewer of its own
   * (the Android WebView). Every page is rendered to a canvas at the
   * container's width, stacked in order.
   */
  import * as pdfjs from 'pdfjs-dist';

  let { bytes }: { bytes: Uint8Array } = $props();

  // The worker ships with the module; vite resolves the URL at build time.
  pdfjs.GlobalWorkerOptions.workerSrc = new URL(
    'pdfjs-dist/build/pdf.worker.min.mjs',
    import.meta.url,
  ).toString();

  let host = $state<HTMLDivElement | null>(null);
  let failure = $state<string | null>(null);
  let pending = $state(true);

  $effect(() => {
    const element = host;
    if (element === null) return;
    let cancelled = false;
    void (async () => {
      try {
        // A copy: pdf.js transfers the buffer it is handed.
        const pdf = await pdfjs.getDocument({ data: bytes.slice() }).promise;
        const drawn: HTMLCanvasElement[] = [];
        for (let number = 1; number <= pdf.numPages; number += 1) {
          const page = await pdf.getPage(number);
          const canvas = document.createElement('canvas');
          const unscaled = page.getViewport({ scale: 1 });
          const width = element.clientWidth > 0 ? element.clientWidth : unscaled.width;
          const viewport = page.getViewport({ scale: width / unscaled.width });
          canvas.width = viewport.width;
          canvas.height = viewport.height;
          await page.render({ canvas, viewport }).promise;
          drawn.push(canvas);
        }
        if (!cancelled) {
          element.append(...drawn);
          pending = false;
        }
      } catch (why) {
        if (!cancelled) {
          failure = `the PDF could not be drawn: ${String(why)}`;
          pending = false;
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  });
</script>

{#if failure !== null}
  <p class="doc-said">{failure}</p>
{:else}
  {#if pending}<p class="pending">Opening the PDF...</p>{/if}
  <div class="doc-pdf-js" bind:this={host}></div>
{/if}

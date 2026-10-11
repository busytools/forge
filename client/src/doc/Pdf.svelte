<script lang="ts">
  /**
   * One PDF, drawn by whichever viewer this surface has: the platform's own
   * over a blob of the bytes, or pdf.js where the surface has none (see
   * `needsPdfJs`). Both read the same bytes the server answered with - no
   * second fetch, no path the page has to reach.
   */
  import { needsPdfJs } from './pdf';
  import PdfJs from './PdfJs.svelte';

  let { bytes }: { bytes: Uint8Array } = $props();

  // A copy over a fresh ArrayBuffer: Blob takes only ArrayBuffer-backed
  // views, and the wire type is the wider `ArrayBufferLike`.
  const url = $derived(
    URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: 'application/pdf' })),
  );

  // The blob goes with the page, or a reader walking through documents
  // leaves one alive per document they opened.
  $effect(() => {
    const held = url;
    return () => URL.revokeObjectURL(held);
  });
</script>

{#if needsPdfJs()}
  <PdfJs {bytes} />
{:else}
  <embed class="doc-pdf" type="application/pdf" src={url} title="PDF document" />
{/if}

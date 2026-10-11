<script lang="ts">
  /**
   * The document viewer: one route, one document, every state drawn.
   *
   * The address is the document (the seat and its resolved path), so a
   * hand-off raises this page and a thread's deep link reopens it. What it
   * draws is the read's answer: markdown as the chat's own prose, text as
   * text, a PDF in the surface's viewer, anything else as itself, and a
   * refusal in the server's own sentence.
   */
  import Prose from '../chat/Prose.svelte';
  import type { DocumentKind } from '../protocol';
  import type { SessionSlot } from '../wire/types';
  import { watchDocument, type DocRead, type DocumentConnection } from './live';
  import Pdf from './Pdf.svelte';

  let {
    slot,
    path,
    reviewable,
    connection,
  }: {
    slot: SessionSlot;
    path: string;
    reviewable: boolean;
    connection: DocumentConnection;
  } = $props();

  const READING: DocRead = {
    state: 'reading',
    path: null,
    kind: null,
    bytes: null,
    reviewable: false,
    refused: null,
  };
  let held = $state<DocRead>(READING);

  $effect(() => {
    const open = connection;
    return watchDocument(open, slot, path, reviewable).subscribe((value) => {
      held = value;
    });
  });

  const text = $derived(held.bytes === null ? '' : new TextDecoder().decode(held.bytes));

  /**
   * A kind the viewer does not draw, said as itself: the extension it
   * carries, or that it carries none.
   */
  function otherLine(kind: DocumentKind | null): string {
    if (kind !== null && typeof kind === 'object') {
      return kind.other === ''
        ? 'This document has no extension, and the viewer draws markdown, PDFs and text.'
        : `This is a .${kind.other} document; the viewer draws markdown, PDFs and text.`;
    }
    return 'The document is not a kind this viewer draws.';
  }
</script>

<main class="wrap doc">
  {#if held.state === 'reading'}
    <p class="pending">Opening {path}...</p>
  {:else if held.state === 'refused'}
    <p class="doc-said">{held.refused}</p>
  {:else if held.bytes !== null && held.bytes.length === 0}
    <p class="doc-empty">The document is empty.</p>
  {:else if held.kind === 'markdown'}
    <article class="doc-body"><Prose {text} /></article>
  {:else if held.kind === 'text'}
    <article class="doc-body"><pre class="doc-text">{text}</pre></article>
  {:else if held.kind === 'pdf'}
    <article class="doc-body"><Pdf bytes={held.bytes ?? new Uint8Array()} /></article>
  {:else}
    <p class="doc-said">{otherLine(held.kind)}</p>
  {/if}
</main>

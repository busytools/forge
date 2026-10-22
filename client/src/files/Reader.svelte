<script lang="ts">
  /**
   * One file, read: the files page's reader.
   *
   * The read is the document read - the seat's cwd IS its tree - taken once
   * when a file is opened and again when it is re-selected, never rewritten
   * under the reader's eyes while a worker edits it (the ruling, 2026-10-17).
   *
   * **What the file IS comes from the one per-format policy**, the same the
   * chat's Read rows read: markdown, code and text draw in the commentable
   * editor, a picture draws itself, a PDF draws its pages, and a file with no
   * viewer draws a notice naming its type and size.
   *
   * **Code-split**: this component pulls `reader.ts` (CodeMirror + shiki)
   * only when a drawable text read has landed, so a page whose reader never
   * opens a file never pays for it.
   */
  import { bytes as size } from '../chat/numbers';
  import type { DocRead } from '../doc/live';
  import { watchDocument } from '../doc/live';
  import Pdf from '../doc/Pdf.svelte';
  import { formatOf } from '../format/policy';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import type { ReaderHandle } from './reader';

  let {
    connection,
    slot,
    path,
    oncomment,
    handle = $bindable(null),
  }: {
    connection: Connection;
    slot: SessionSlot;
    path: string;
    /** A line was pressed: the composer lands on it (the rail's half). */
    oncomment?: (line: number) => void;
    /** The live editor, so the page's own controls can jump to a line. */
    handle?: ReaderHandle | null;
  } = $props();

  let read = $state<DocRead | null>(null);
  let mount = $state<HTMLElement | null>(null);
  let width = $state(0);
  let height = $state(0);

  $effect(() => {
    read = null;
    width = 0;
    height = 0;
    return watchDocument(connection, slot, path, false).subscribe(($next) => {
      read = $next;
    });
  });

  /** What the file is, from its path and its own bytes. */
  const format = $derived(read?.bytes == null ? null : formatOf(path, read.bytes));
  /** The formats the commentable editor draws, which are the text kinds. */
  const drawable = $derived(
    format !== null &&
      (format.kind === 'markdown' || format.kind === 'code' || format.kind === 'text'),
  );
  const text = $derived(
    drawable && read?.bytes != null ? new TextDecoder().decode(read.bytes) : null,
  );
  const lines = $derived(text === null ? 0 : text.split('\n').length);
  /** The grammar named in the head, or markdown, which the editor also draws. */
  const language = $derived(
    format === null
      ? null
      : format.kind === 'code'
        ? format.language
        : format.kind === 'markdown'
          ? 'markdown'
          : null,
  );
  /** The image's bytes as a URL the browser can draw, released with the page. */
  const picture = $derived.by(() => {
    if (format?.kind !== 'image' || read?.bytes == null) return null;
    return URL.createObjectURL(new Blob([new Uint8Array(read.bytes)], { type: format.mime }));
  });
  $effect(() => {
    const url = picture;
    return () => {
      if (url !== null) URL.revokeObjectURL(url);
    };
  });

  /** The head's right-hand fact: the language and lines, or why there is none. */
  const fact = $derived.by(() => {
    if (format === null) return null;
    if (format.kind === 'text') return 'plain text - no syntax for this file';
    if (format.kind === 'markdown' || format.kind === 'code') {
      return `${language ?? 'text'} \u{b7} ${lines} ${lines === 1 ? 'line' : 'lines'}`;
    }
    if (format.kind === 'image') {
      const dims = width > 0 ? `${width} \u{d7} ${height}` : size(read?.bytes?.length ?? 0);
      return `${format.mime} \u{b7} ${dims}`;
    }
    if (format.kind === 'pdf') return `application/pdf \u{b7} ${size(read?.bytes?.length ?? 0)}`;
    return null;
  });

  // Build the editor when a drawable text lands; the dynamic import is the
  // chunk boundary, and the cleanup lets the old instance go on a re-read.
  $effect(() => {
    const el = mount;
    const body = text;
    if (!drawable || body === null || el === null) return;
    let live = true;
    let built: ReaderHandle | null = null;
    void import('./reader').then(async ({ buildReader }) => {
      if (!live) return;
      built = await buildReader(el, body, path, oncomment);
      if (!live) {
        built.destroy();
        return;
      }
      handle = built;
    });
    return () => {
      live = false;
      built?.destroy();
      handle = null;
    };
  });
</script>

<section class="files-reader" aria-label="the file">
  <header class="files-filehead">
    <span class="files-path">{path}</span>
    {#if fact !== null}
      <span class="files-fact">{fact}</span>
    {/if}
  </header>
  {#if read === null || read.state === 'reading'}
    <p class="files-quiet">Reading {path}...</p>
  {:else if read.state === 'refused'}
    <p class="files-quiet">{read.refused}</p>
  {:else if format === null}
    <p class="files-quiet">The read answered no bytes for this file.</p>
  {:else if drawable}
    <!-- The door has to be VISIBLE before it is used (Ved, 2026-10-17): a
         line takes a comment, and nothing said so until a press. -->
    <p class="files-hint">press a line to comment</p>
    <div class="files-editor" bind:this={mount}></div>
  {:else if format.kind === 'image'}
    <div class="files-media">
      {#if picture !== null}
        <img src={picture} alt={path} bind:naturalWidth={width} bind:naturalHeight={height} />
      {/if}
    </div>
  {:else if format.kind === 'pdf'}
    <div class="files-media">
      <Pdf bytes={read.bytes ?? new Uint8Array()} />
    </div>
  {:else if format.kind === 'binary'}
    <p class="files-notice">
      This file is <b>not text</b> ({format.mime}, {size(read.bytes?.length ?? 0)}), and there is no
      viewer for it here. It is still in the tree and reviewable as a file.
    </p>
  {:else}
    <!-- Unreachable while the policy's kinds are all drawn above; a kind it
         grows still says itself rather than drawing a blank (rule 25). -->
    <p class="files-quiet">This file is not a kind this page draws.</p>
  {/if}
</section>

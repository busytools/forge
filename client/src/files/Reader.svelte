<script lang="ts">
  /**
   * One file, read: the files page's reader.
   *
   * The read is the document read - the seat's cwd IS its tree - taken once
   * when a file is opened and again when it is re-selected, never rewritten
   * under the reader's eyes while a worker edits it (the ruling, 2026-10-17).
   *
   * **What the file IS comes from the one per-format policy**, the same the
   * chat's Read rows read: markdown renders as the document viewer's blocks,
   * code and text draw in the commentable editor, a picture draws itself, a
   * PDF draws its pages, and a file with no viewer draws a notice naming its
   * type and size.
   *
   * **Code-split**: this component pulls `reader.ts` (CodeMirror + shiki)
   * only when a commentable code or text read has landed, so a page whose
   * reader never opens one never pays for it. Markdown renders through the
   * shared prose renderer instead and pulls no editor.
   */
  import type { Snippet } from 'svelte';

  import { bytes as size } from '../chat/numbers';
  import { linesOf, type DocRow } from '../doc/comments';
  import type { DocRead } from '../doc/live';
  import { watchDocument } from '../doc/live';
  import Pdf from '../doc/Pdf.svelte';
  import { formatOf } from '../format/policy';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { markdownRows } from './markdown';
  import type { ReaderHandle } from './reader';

  let {
    connection,
    slot,
    path,
    oncomment,
    onblock,
    under,
    handle = $bindable(null),
  }: {
    connection: Connection;
    slot: SessionSlot;
    path: string;
    /** A line was pressed: the composer lands on it (the rail's half). */
    oncomment?: (line: number) => void;
    /** A rendered block was pressed: the composer lands on its first line. */
    onblock?: (row: DocRow) => void;
    /** What the page draws under a rendered block: its thread cards and the
     *  open composer, the same blocks it mounts into the editor. The second
     *  argument marks the first row, which also takes a block anchored in a
     *  hidden frontmatter block; the third marks the last row, which takes
     *  one placed past the frozen snapshot's end. */
    under?: Snippet<[DocRow, boolean, boolean]>;
    /** The live reader, so the page's own controls can jump to a line. */
    handle?: ReaderHandle | null;
  } = $props();

  let read = $state<DocRead | null>(null);
  let mount = $state<HTMLElement | null>(null);
  let width = $state(0);
  let height = $state(0);
  /** Why the reader could not be built, when the chunk or a grammar failed. */
  let buildError = $state<string | null>(null);

  $effect(() => {
    read = null;
    width = 0;
    height = 0;
    buildError = null;
    return watchDocument(connection, slot, path, false).subscribe(($next) => {
      read = $next;
    });
  });

  /** What the file is, from its path and its own bytes. */
  const format = $derived(read?.bytes == null ? null : formatOf(path, read.bytes));
  /** **Markdown renders; code and text keep the commentable editor.** */
  const markdown = $derived(format?.kind === 'markdown');
  const commentable = $derived(format?.kind === 'code' || format?.kind === 'text');
  const text = $derived(
    (markdown || commentable) && read?.bytes != null ? new TextDecoder().decode(read.bytes) : null,
  );
  /** Markdown's own comment targets: one rendered block each, frontmatter hidden. */
  const rendered = $derived(markdown && text !== null ? markdownRows(text) : []);
  const lines = $derived(text === null ? 0 : text.split('\n').length);
  /** The grammar named in the head: a code file's, or markdown. */
  const language = $derived(
    format === null
      ? null
      : format.kind === 'code'
        ? format.language
        : format.kind === 'markdown'
          ? 'markdown'
          : null,
  );
  /**
   * The grammar the editor draws with.
   *
   * Handed to `buildReader` rather than re-derived there: only this
   * component holds the bytes, and an extensionless shebang script's
   * language is only readable from them. Markdown never reaches it - its
   * blocks are rendered instead of edited.
   */
  const grammar = $derived(
    format === null ? 'text' : format.kind === 'code' ? format.language : 'text',
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
    if (format.kind === 'binary') {
      // The mock's own word for an archive, and the mime for everything else.
      const name =
        format.mime.includes('zip') || format.mime.includes('tar') || format.mime.includes('gzip')
          ? 'archive'
          : format.mime;
      return `${name} \u{b7} ${size(read?.bytes?.length ?? 0)}`;
    }
    return null;
  });

  // Build the editor when a commentable text lands; the dynamic import is the
  // chunk boundary, and the cleanup lets the old instance go on a re-read.
  $effect(() => {
    const el = mount;
    const body = text;
    if (!commentable || body === null || el === null) return;
    let live = true;
    let built: ReaderHandle | null = null;
    void import('./reader')
      .then(async ({ buildReader }) => {
        if (!live) return;
        built = await buildReader(el, body, grammar, oncomment);
        if (!live) {
          built.destroy();
          return;
        }
        handle = built;
      })
      // A failed chunk or grammar is drawn, never left as an empty editor.
      .catch((why: unknown) => {
        if (live) buildError = why instanceof Error ? why.message : String(why);
      });
    return () => {
      live = false;
      built?.destroy();
      handle = null;
    };
  });

  /** Where the rendered markdown draws, so a jump can find its blocks. */
  let blockHost = $state<HTMLElement | null>(null);

  /**
   * **A rendered markdown file hands the page the same handle the editor
   * does.** `lines()` is the raw file's lines - what an anchor hashes and
   * captures from - and a jump scrolls to the block whose span holds the
   * line. The page draws its thread cards and composer through `under`
   * instead of `setBlocks`, so that half is a no-op here.
   */
  $effect(() => {
    if (!markdown || text === null) return;
    const body = text;
    handle = {
      destroy: () => {},
      targetLine: (line) => {
        const host = blockHost;
        if (line === null || host === null) return;
        const rows = [...host.querySelectorAll<HTMLElement>('[data-line]')];
        const target =
          rows.find((row) => {
            const first = Number(row.dataset.line);
            const last = Number(row.dataset.end);
            return line >= first && line <= last;
          }) ??
          // **A thread anchored in the hidden frontmatter draws under the
          // first block** (the page's own fallback), so a jump to its line
          // lands there rather than nowhere.
          (rows[0] !== undefined && line < Number(rows[0].dataset.line) ? rows[0] : undefined);
        target?.scrollIntoView?.({ block: 'center' });
      },
      setBlocks: () => {},
      lines: () => linesOf(body).map((line) => line.text),
    };
    return () => {
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
  {:else if markdown}
    <!-- **Markdown renders; there is no source view.** Each rendered block
         is a comment target with a `+` on its first source line, drawn the
         way the document hand-off viewer draws its blocks. -->
    <p class="files-hint">press a block to comment</p>
    <div class="files-markdown" bind:this={blockHost}>
      <article class="files-blocks doc-blocks">
        {#each rendered as row, at (row.line)}
          <!-- The row itself is the target: a pointer press opens the
               composer, and the keyboard reaches it with tabindex + Enter.
               svelte-check wants role=button for that, which would nest any
               link the rendered markdown carries inside a button - so the
               row is a focusable group carrying its own action instead. -->
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <div
            class="doc-block prose"
            data-line={row.line}
            data-end={row.end}
            role="group"
            tabindex="0"
            aria-label="comment on this part of the file"
            onclick={(event) => {
              // A link inside the block is the link's, and the `+` is its
              // own target: neither press also opens the block's composer.
              if (event.target instanceof Element && event.target.closest('a, button') !== null) {
                return;
              }
              onblock?.(row);
            }}
            onkeydown={(event) => {
              // The keyboard half of the link rule: Enter/Space on a link or
              // the `+` reaches that control, never the block.
              if (event.target instanceof Element && event.target.closest('a, button') !== null) {
                return;
              }
              if (event.key === 'Enter' || event.key === ' ') {
                event.preventDefault();
                onblock?.(row);
              }
            }}
          >
            <!-- The block's own line is the line-level target: a `+` shown
                 on hover or focus, drawn always for a finger. -->
            <button
              class="doc-add"
              type="button"
              aria-label={`comment on line ${row.line}`}
              onclick={(event) => {
                event.stopPropagation();
                oncomment?.(row.line);
              }}>+</button
            >
            <div class="doc-block-body">
              {#if row.html !== null}
                <!-- The rendered markdown, produced by the shared renderer
                     from escaped input: raw HTML off, code panels, stacked
                     tables - the chat's own rules. -->
                <!-- eslint-disable-next-line svelte/no-at-html-tags -->
                {@html row.html}
              {:else}
                <span class="doc-raw">{row.text === '' ? ' ' : row.text}</span>
              {/if}
            </div>
          </div>
          {#if under}{@render under(row, at === 0, at === rendered.length - 1)}{/if}
        {/each}
      </article>
    </div>
  {:else if commentable && buildError !== null}
    <!-- A chunk or grammar that failed is a state, not an empty editor. -->
    <p class="files-quiet">the reader could not be built: {buildError}</p>
  {:else if commentable}
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

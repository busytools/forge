<script lang="ts">
  /**
   * One file, read: the files page's reader.
   *
   * The read is the document read - the seat's cwd IS its tree - taken once
   * when a file is opened and again when it is re-selected, never rewritten
   * under the reader's eyes while a worker edits it (the ruling, 2026-10-17).
   *
   * **Code-split**: this component pulls `reader.ts` (CodeMirror + shiki)
   * only when a drawable text read has landed, so a page whose reader never
   * opens a file never pays for it.
   */
  import type { DocRead } from '../doc/live';
  import { watchDocument } from '../doc/live';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { drawableText, languageForPath } from './languages';
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

  $effect(() => {
    read = null;
    return watchDocument(connection, slot, path, false).subscribe(($next) => {
      read = $next;
    });
  });

  const text = $derived(read?.bytes == null ? null : new TextDecoder().decode(read.bytes));
  const drawable = $derived(read !== null && drawableText(read.kind, read.reviewable));
  const lines = $derived(text === null ? 0 : text.split('\n').length);
  /** The language the reader colours this path as, named in the head. */
  const language = $derived(languageForPath(path)?.shiki ?? null);

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
    {#if drawable}
      <span class="files-fact">
        {#if language !== null}{language}<span class="files-sep">|</span>{/if}{lines} lines
      </span>
    {/if}
  </header>
  {#if read === null || read.state === 'reading'}
    <p class="files-quiet">Reading {path}...</p>
  {:else if read.state === 'refused'}
    <p class="files-quiet">{read.refused}</p>
  {:else if !drawable}
    <p class="files-quiet">This page draws no text for this kind of file.</p>
  {:else}
    <!-- The door has to be VISIBLE before it is used (Ved, 2026-10-17): a
         line takes a comment, and nothing said so until a press. -->
    <p class="files-hint">press a line to comment</p>
    <div class="files-editor" bind:this={mount}></div>
  {/if}
</section>

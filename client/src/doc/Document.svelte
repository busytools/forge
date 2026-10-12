<script lang="ts">
  /**
   * The document viewer: one route, one document, every state drawn.
   *
   * The address is the document (the seat and its resolved path), so a
   * hand-off raises this page and a thread's deep link reopens it. What it
   * draws is the read's answer: markdown as the chat's own prose, text as
   * text, a PDF in the surface's viewer, anything else as itself, and a
   * refusal in the server's own sentence.
   *
   * **A reviewable document draws as its LINES**, because a comment anchors
   * to the raw file's numbering, not to a rendered block's: every line is a
   * target, a comment waits under the line it is about, and the finish
   * files them as one review - the same event a code review leaves, through
   * the same commands. A comment written against another version of the
   * document draws outdated AT ONCE, at its captured lines, and nothing
   * re-anchors it.
   */
  import { report } from '../socket';
  import Prose from '../chat/Prose.svelte';
  import type { DocumentKind } from '../protocol';
  import { authorName } from '../review/view';
  import { submitCommand, upsertCommand } from '../review/write';
  import type { SessionSlot } from '../wire/types';
  import {
    addDraft,
    clearDrafts,
    contextAround,
    docScope,
    docThread,
    draftsFor,
    linesOf,
    placedThreads,
    threadsAgainstAnotherVersion,
    type DocDraft,
  } from './comments';
  import { watchDocument, type DocRead, type DocumentConnection } from './live';
  import Pdf from './Pdf.svelte';

  /** Why a command was refused, as a sentence: the socket rejects with the
   *  server's own string; anything else is spoken as itself. */
  function reasonOf(why: unknown): string {
    if (typeof why === 'string' && why !== '') return why;
    if (why instanceof Error) return why.message;
    return String(why);
  }

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
    contentHash: null,
    threads: [],
    reviews: [],
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

  /** The scope this document's comments are stored under. */
  const scope = $derived(docScope(held.path ?? path));

  /** The unsent comments on this document, read from the module map so they
   *  survive the viewer leaving and a second hand-off arriving. The version
   *  ticks after every mutation, because the map itself is not reactive. */
  let draftsVersion = $state(0);
  const drafts = $derived.by(() => {
    void draftsVersion;
    return draftsFor(scope);
  });

  const hashed = $derived(held.contentHash ?? '');
  const placed = $derived(placedThreads(held.threads, hashed));
  const outdated = $derived(threadsAgainstAnotherVersion(held.threads, hashed));

  /** The line a composer is open on, and what is typed into it. */
  let composing = $state<number | null>(null);
  let commentText = $state('');

  let finishState = $state<
    | { kind: 'idle' | 'busy' }
    | { kind: 'left'; number: number | null }
    | { kind: 'refused'; why: string }
  >({ kind: 'idle' });

  function openComposer(line: number): void {
    composing = composing === line ? null : line;
    commentText = '';
  }

  /** Write one comment: held as a draft here, upserted to the store the
   *  moment it is written - the finish is what files it into a review. */
  function writeComment(line: number): void {
    const written = commentText.trim();
    if (written === '' || held.path === null || held.contentHash === null) return;
    const draft: DocDraft = {
      id: crypto.randomUUID(),
      line,
      text: written,
      context: contextAround(linesOf(text), line),
    };
    addDraft(scope, draft);
    draftsVersion += 1;
    try {
      void connection
        ?.dispatch(upsertCommand(slot, scope, docThread(draft, held.path, held.contentHash)))
        ?.catch((why: unknown) => {
          report('a comment could not be stored', why);
        });
    } catch (why) {
      report('a comment could not be sent', why);
    }
    composing = null;
    commentText = '';
  }

  /** File the drafts as one review, and draw what came back: the number the
   *  round minted, or the refusal in the core's own words. */
  async function finish(): Promise<void> {
    if (drafts.length === 0 || held.path === null) return;
    const ids = drafts.map((draft) => draft.id);
    finishState = { kind: 'busy' };
    let answer: unknown;
    try {
      answer = await connection.dispatch(submitCommand(slot, scope, ids, null), slot);
    } catch (why) {
      finishState = { kind: 'refused', why: reasonOf(why) };
      return;
    }
    if (typeof answer === 'string') {
      finishState = { kind: 'refused', why: answer };
      return;
    }
    if (answer === null || answer === undefined) {
      finishState = { kind: 'refused', why: 'the review could not be filed' };
      return;
    }
    clearDrafts(scope, ids);
    draftsVersion += 1;
    const number = (answer as { number?: unknown }).number;
    finishState = { kind: 'left', number: typeof number === 'number' ? number : null };
  }

  const lines = $derived(linesOf(text));

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
  {:else if held.kind === 'markdown' && !held.reviewable}
    <article class="doc-body"><Prose {text} /></article>
  {:else if held.kind === 'text' && !held.reviewable}
    <article class="doc-body"><pre class="doc-text">{text}</pre></article>
  {:else if held.kind === 'pdf'}
    <article class="doc-body"><Pdf bytes={held.bytes ?? new Uint8Array()} /></article>
  {:else if held.kind === 'markdown' || held.kind === 'text'}
    <!-- A reviewable document: its own lines, each one a target, so a
         comment anchors to the raw file's numbering. -->
    <article class="doc-body doc-lines">
      {#each lines as line (line.number)}
        <div class="doc-line" class:target={composing === line.number}>
          <button
            class="doc-lnum"
            aria-label="comment on line {line.number}"
            onclick={() => openComposer(line.number)}>{line.number}</button
          >
          <span class="doc-ltext">{line.text === '' ? ' ' : line.text}</span>
        </div>
        {#each placed.filter((thread) => thread.anchor.line === line.number) as thread (thread.id)}
          <div class="doc-note">
            <span class="doc-who"
              >{authorName(thread.comments[0]?.author ?? { kind: 'unknown', raw: null })}</span
            >
            <span class="doc-words">{thread.comments[0]?.text ?? ''}</span>
          </div>
        {/each}
        {#each drafts.filter((draft) => draft.line === line.number) as draft (draft.id)}
          <div class="doc-note doc-draft">
            <span class="doc-who">you</span>
            <span class="doc-words">{draft.text}</span>
            <span class="doc-cc">drafted</span>
          </div>
        {/each}
        {#if composing === line.number}
          <div class="doc-compose">
            <!-- A take's words are routed nowhere here: a comment is typed. -->
            <textarea
              data-editor="nowhere"
              aria-label="comment on line {line.number}"
              bind:value={commentText}
              placeholder="comment on line {line.number}..."></textarea>
            <div class="doc-compose-acts">
              <button
                class="doc-comment-btn"
                disabled={commentText.trim() === ''}
                onclick={() => writeComment(line.number)}>comment</button
              >
              <button class="doc-cancel-btn" onclick={() => openComposer(line.number)}
                >cancel</button
              >
            </div>
          </div>
        {/if}
      {/each}

      {#if outdated.length > 0}
        <section class="doc-stale">
          <h2>written against an earlier version of this document</h2>
          {#each outdated as thread (thread.id)}
            <div class="doc-note doc-quoted">
              <div class="doc-quote">
                {#each thread.anchor.context as captured, at (at)}<div>{captured}</div>{/each}
              </div>
              <span class="doc-who"
                >{authorName(thread.comments[0]?.author ?? { kind: 'unknown', raw: null })}</span
              >
              <span class="doc-words">{thread.comments[0]?.text ?? ''}</span>
            </div>
          {/each}
        </section>
      {/if}

      <div class="doc-finish">
        {#if finishState.kind === 'left'}
          <p class="doc-left">
            review left{finishState.number === null ? '' : ` (R${finishState.number})`} - the agent hears
            it like any review.
          </p>
        {:else}
          <button
            class="doc-finish-btn"
            disabled={drafts.length === 0 || finishState.kind === 'busy'}
            onclick={() => void finish()}
            >finish review{drafts.length > 0 ? ` (${drafts.length})` : ''}</button
          >
          {#if finishState.kind === 'refused'}
            <p class="doc-said">{finishState.why}</p>
          {/if}
        {/if}
      </div>
    </article>
  {:else}
    <p class="doc-said">{otherLine(held.kind)}</p>
  {/if}
</main>

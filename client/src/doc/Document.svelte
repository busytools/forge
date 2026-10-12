<script lang="ts">
  /**
   * The document viewer: one route, one document, every state drawn.
   *
   * The address is the document (the seat and its resolved path), so a
   * hand-off's Open raises this page and a thread's deep link reopens it.
   * What it draws is the read's answer: markdown as the chat's own prose,
   * text as text, a PDF in the surface's viewer, anything else as itself,
   * and a refusal in the server's own sentence.
   *
   * **A reviewable document draws as its rendered BLOCKS, each one a
   * comment target**: hover (or focus) marks the block under the pointer,
   * a press opens the composer, and the comment anchors to the block's own
   * source lines - which is what keeps the captured lines and the
   * outdated marking exact when the document changes. A plain-text document
   * has no blocks, so its rows are its lines.
   *
   * **The done is the hand-off's answer.** The reader comments, presses
   * done, and the drafts are filed as one review through the same commands
   * a code review rides - then the parked call is released with what was
   * filed, so the agent hears the reading's result and not merely that the
   * document opened.
   */
  import { report } from '../socket';
  import Prose from '../chat/Prose.svelte';
  import type { DocumentKind } from '../protocol';
  import { authorName } from '../review/view';
  import { statusCommand, submitCommand, upsertCommand } from '../review/write';
  import type { SessionSlot } from '../wire/types';
  import {
    addDraft,
    clearDrafts,
    docScope,
    docThread,
    draftsFor,
    rowsOf,
    placedThreads,
    threadsAgainstAnotherVersion,
    type DocDraft,
    type DocRow,
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
    handoff = null,
    connection,
  }: {
    slot: SessionSlot;
    path: string;
    reviewable: boolean;
    /** The parked hand-off's id, when this route came from an ask's Open;
     *  null on a deep link, where there is no call to release. */
    handoff?: string | null;
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
    // A re-hand carries a NEW id, and that is a fresh open: the same
    // document can have changed under the reader, so the read runs again.
    void handoff;
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

  /** The comment targets: rendered blocks for markdown, lines for text. */
  const rows = $derived(held.kind === 'text' ? rowsOf(text, 'text') : rowsOf(text, 'markdown'));

  /**
   * The drift is written back where it is drawn: a diff's overlay ages a
   * drifted code thread as it re-anchors it, and this viewer is the
   * overlay's counterpart for a document, so the review record reads what
   * the reader sees. Resolved and already-outdated comments are left alone.
   */
  $effect(() => {
    if (!held.reviewable || held.path === null) return;
    for (const thread of outdated) {
      if (thread.status !== 'open' && thread.status !== 'addressed') continue;
      void connection
        ?.dispatch(statusCommand(slot, scope, thread.id, 'Outdated'))
        ?.catch((why: unknown) => {
          report('a comment could not be aged', why);
        });
    }
  });

  /** The block a composer is open on, and what is typed into it. */
  let composing = $state<number | null>(null);
  let commentText = $state('');

  let finishState = $state<
    | { kind: 'idle' | 'busy' }
    | { kind: 'done'; number: number | null }
    | { kind: 'refused'; why: string }
  >({ kind: 'idle' });

  function openComposer(row: DocRow): void {
    composing = composing === row.line ? null : row.line;
    commentText = '';
  }

  /** Write one comment: held as a draft here, upserted to the store the
   *  moment it is written - the done is what files it into a review. */
  function writeComment(row: DocRow): void {
    const written = commentText.trim();
    if (written === '' || held.path === null || held.contentHash === null) return;
    const draft: DocDraft = {
      id: crypto.randomUUID(),
      line: row.line,
      text: written,
      context: row.context,
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

  /**
   * The reader is done: file the drafts as one review (through the same
   * command a code review rides), then release the parked hand-off with
   * what was filed - a refusal keeps the drafts and says why, and nothing
   * is answered until the review actually landed.
   */
  async function done(): Promise<void> {
    if (held.path === null) return;
    finishState = { kind: 'busy' };
    let reviewId: string | null = null;
    let number: number | null = null;
    if (drafts.length > 0) {
      const ids = drafts.map((draft) => draft.id);
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
      const filed = answer as { id?: unknown; number?: unknown };
      reviewId = typeof filed.id === 'string' ? filed.id : null;
      number = typeof filed.number === 'number' ? filed.number : null;
    }
    if (handoff !== null) {
      try {
        await connection.dispatch(
          {
            answer_document_hand_off: {
              key: slot,
              id: handoff,
              answer: { done: { review_id: reviewId } },
            },
          },
          slot,
        );
      } catch (why) {
        finishState = { kind: 'refused', why: reasonOf(why) };
        return;
      }
    }
    finishState = { kind: 'done', number };
  }

  /** The threads a row is the home of: their anchor's line is inside it. */
  function threadsUnder(row: DocRow) {
    return placed.filter(
      (thread) => thread.anchor.line >= row.line && thread.anchor.line <= row.end,
    );
  }

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
    <!-- A reviewable document: its rendered blocks, each one a comment
         target, so a comment anchors to the block it was written on. -->
    <article class="doc-body doc-blocks">
      {#each rows as row (row.line)}
        <!-- The row itself is the target (Ved's shape): a pointer press
             opens the composer, and the keyboard reaches it with tabindex +
             Enter. svelte-check wants role=button for that, which would nest
             any link the rendered markdown carries inside a button - so the
             row is a focusable group carrying its own action instead. -->
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
        <div
          class="doc-block"
          class:target={composing === row.line}
          role="group"
          tabindex="0"
          aria-label="comment on this part of the document"
          onclick={() => openComposer(row)}
          onkeydown={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              openComposer(row);
            }
          }}
        >
          {#if row.html !== null}
            <!-- The rendered markdown, which `renderProseBlocks` produced
                 from escaped input: the same renderer and the same rules
                 the chat's own Prose draws with, raw HTML off. -->
            <!-- eslint-disable-next-line svelte/no-at-html-tags -->
            {@html row.html}
          {:else}
            <span class="doc-raw">{row.text === '' ? ' ' : row.text}</span>
          {/if}
        </div>
        {#each threadsUnder(row) as thread (thread.id)}
          <div class="doc-note">
            <span class="doc-who"
              >{authorName(thread.comments[0]?.author ?? { kind: 'unknown', raw: null })}</span
            >
            <span class="doc-words">{thread.comments[0]?.text ?? ''}</span>
          </div>
        {/each}
        {#each drafts.filter((draft) => draft.line === row.line) as draft (draft.id)}
          <div class="doc-note doc-draft">
            <span class="doc-who">you</span>
            <span class="doc-words">{draft.text}</span>
            <span class="doc-cc">drafted</span>
          </div>
        {/each}
        {#if composing === row.line}
          <div class="doc-compose">
            <!-- A take's words are routed nowhere here: a comment is typed. -->
            <textarea
              data-editor="nowhere"
              aria-label="comment on this part of the document"
              bind:value={commentText}
              placeholder="comment on this part..."></textarea>
            <div class="doc-compose-acts">
              <button
                class="doc-comment-btn"
                disabled={commentText.trim() === ''}
                onclick={() => writeComment(row)}>comment</button
              >
              <button class="doc-cancel-btn" onclick={() => openComposer(row)}>cancel</button>
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

      {#if drafts.length > 0 || handoff !== null}
        <div class="doc-finish">
          {#if finishState.kind === 'done'}
            <p class="doc-left">
              done{finishState.number === null
                ? ''
                : ` - review left (R${finishState.number})`}{handoff === null
                ? '.'
                : ' - the agent has it.'}
            </p>
          {:else}
            <button
              class="doc-finish-btn"
              disabled={finishState.kind === 'busy'}
              onclick={() => void done()}
              >done{drafts.length > 0 ? ` (${drafts.length})` : ''}</button
            >
            {#if finishState.kind === 'refused'}
              <p class="doc-said">{finishState.why}</p>
            {/if}
          {/if}
        </div>
      {/if}
    </article>
  {:else}
    <p class="doc-said">{otherLine(held.kind)}</p>
  {/if}
</main>

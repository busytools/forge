<script lang="ts">
  /**
   * The files page: one seat's whole working tree, walked file by file, with
   * comments droppable on any line and the dropped set listed beside the
   * reader.
   *
   * It subscribes the seat itself, as the review page does, and answers
   * nothing. **The tree follows the SEAT** - a worker's own worktree, the
   * lead's checkout - so the page is keyed to the slot and never to a branch
   * the tree happens to be on.
   *
   * **The writes are the review page's own** (`review/write.ts`): a comment
   * is an `upsert_review_thread` under the seat's TREE scope, a reply the
   * same with the reviewer's turn appended, a resolve a
   * `set_review_thread_status`, and `finish review` a `submit_review` that
   * seals the sitting into one round and nudges the seat with `Review #N
   * ready`. After every write the record is re-read, because the reviews
   * ride the record and nothing pushes a review change.
   *
   * Its states are drawn here: nothing yet (reading), a seat the server
   * refused, and a tree with no files.
   */
  import { SvelteSet } from 'svelte/reactivity';
  import { mount, unmount } from 'svelte';

  import FileTree from '../components/FileTree.svelte';
  import { toggleClosed, type TreeRow } from '../components/tree';
  import { filesFrom, filesOnly } from '../composer/wire';
  import { hrefForSlot } from '../routes';
  import {
    finishRound,
    newThread,
    reopenThread,
    statusCommand,
    upsertCommand,
    withReply,
    type Sends,
    type SendOutcome,
  } from '../review/write';
  import { latestRound, roundHasWork } from '../review/view';
  import { watchSession, type SessionRead } from '../session/live';
  import type { ReviewThreadRecord, SessionRecord } from '../session/wire';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import Box from './Box.svelte';
  import Inline from './Inline.svelte';
  import Rail from './Rail.svelte';
  import Reader from './Reader.svelte';
  import { contextForComment, railRows, treeScope, type CommentSpot, type Shelf } from './comments';
  import type { ReaderBlock, ReaderHandle } from './reader';
  import { indexTreeRows, matchedEntries } from './tree';

  let { connection, slot }: { connection: Connection; slot: SessionSlot } = $props();

  let read = $state<SessionRead>({ wire: null, refused: null });

  $effect(() => {
    return watchSession(connection, slot, false).subscribe(($next) => {
      read = $next;
    });
  });

  const record: SessionRecord | null = $derived(read.wire);
  /** The seat's files, directories dropped: the walk lists both, and the
   *  tree derives its own directories. */
  const entries = $derived(record === null ? [] : filesOnly(filesFrom(record.file_index)));

  let query = $state('');
  const closed = new SvelteSet<string>();
  /** The file the reader shows, which the tree marks as current. */
  let open = $state<string | null>(null);
  /** The live reader, so the rail's jump-backs can reach its lines. */
  let reader = $state<ReaderHandle | null>(null);
  /** A jump's line, applied once the reader for its file is in hand. */
  let pendingLine = $state<number | null>(null);

  /** The seat-tree half of the record: the files page's threads and rounds. */
  const treeThreads = $derived(
    record?.tree_reviews.threads.outcome === 'read' ? record.tree_reviews.threads.value : [],
  );
  const treeRounds = $derived(
    record?.tree_reviews.reviews.outcome === 'read' ? record.tree_reviews.reviews.value : [],
  );
  const rows = $derived(railRows(treeThreads));
  /** Threads the worker has answered and nobody read yet - the header count. */
  const repliesToRead = $derived(
    treeThreads.filter(
      (thread) =>
        (thread.status === 'addressed' || thread.status === 'outdated') &&
        thread.comments[thread.comments.length - 1]?.author.kind === 'agent',
    ).length,
  );

  // The page's boxes, held here because the reader is re-mounted under them.
  const comment = $state<Shelf & { spot: CommentSpot | null }>({
    spot: null,
    draft: '',
    busy: false,
  });
  const reply = $state<Shelf & { thread: string | null }>({
    thread: null,
    draft: '',
    busy: false,
  });
  /** The unfiled turns of this sitting: what `finish review` seals. */
  const drafts = new SvelteSet<string>();
  let busy = $state(false);
  let refusal = $state<string | null>(null);
  let finishOpen = $state(false);
  let now = $state(Date.now());

  /**
   * Which side panel a narrow screen has raised, or null. The tree and the
   * comments are columns when the width is there and sheets when it is not,
   * and every raising takes a history entry so Android's Back closes what
   * it opened (the board's drawer is the pattern).
   */
  let sheet = $state<'tree' | 'comments' | null>(null);
  const SHEET_ENTRY = 'forgeFilesSheet';

  function heldState(): Record<string, unknown> {
    const held: unknown = history.state;
    return typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
  }

  function openSheet(which: 'tree' | 'comments'): void {
    if (sheet === null) {
      history.pushState({ ...heldState(), [SHEET_ENTRY]: true }, '', location.href);
    }
    sheet = which;
  }

  /** Close it; the entry on top is stepped back through, one close path. */
  function closeSheet(): void {
    if (heldState()[SHEET_ENTRY] === true) {
      history.back();
      return;
    }
    sheet = null;
  }

  $effect(() => {
    const restore = () => {
      if (sheet !== null) sheet = null;
    };
    addEventListener('popstate', restore);
    return () => removeEventListener('popstate', restore);
  });

  $effect(() => {
    if (sheet === null) return;
    const onkey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') closeSheet();
    };
    addEventListener('keydown', onkey);
    return () => removeEventListener('keydown', onkey);
  });

  /**
   * The tree's keyboard (Ved, 2026-10-17): the arrows walk the rows WITHOUT
   * a prior focus - pressing one from anywhere on the page marks the row it
   * lands on (the tree's own mark, never the browser's ring) and moving
   * onto a file opens it. Home and End go to the ends, left and right fold
   * a directory, Enter opens or folds. The code pane and any text box keep
   * their own arrows.
   */
  let keyRow = $state<string | null>(null);

  $effect(() => {
    const typingSomewhere = (target: EventTarget | null): boolean =>
      target instanceof HTMLElement &&
      (target.closest('.cm-editor') !== null ||
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target.isContentEditable);
    const land = (row: TreeRow<unknown> | undefined): void => {
      if (row === undefined) return;
      keyRow = row.path;
      if (row.kind === 'file') open = row.path;
    };
    const step = (delta: number): void => {
      const paths = tree.map((row) => row.path);
      if (paths.length === 0) return;
      const from = keyRow === null ? paths.indexOf(open ?? '') : paths.indexOf(keyRow);
      const at = Math.min(Math.max((from === -1 ? -1 : from) + delta, 0), paths.length - 1);
      land(tree[at]);
    };
    const onkey = (event: KeyboardEvent): void => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (typingSomewhere(event.target)) return;
      switch (event.key) {
        case 'ArrowDown':
          event.preventDefault();
          step(1);
          break;
        case 'ArrowUp':
          event.preventDefault();
          step(-1);
          break;
        case 'Home':
          event.preventDefault();
          land(tree[0]);
          break;
        case 'End':
          event.preventDefault();
          land(tree.at(-1));
          break;
        case 'ArrowLeft':
        case 'ArrowRight': {
          const row = tree.find((held) => held.path === keyRow);
          if (row?.kind !== 'dir') return;
          event.preventDefault();
          const isClosed = closed.has(row.path);
          if (
            (event.key === 'ArrowRight' && isClosed) ||
            (event.key === 'ArrowLeft' && !isClosed)
          ) {
            toggleClosed(closed, row.path);
          }
          break;
        }
        case 'Enter': {
          const row = tree.find((held) => held.path === keyRow);
          if (row === undefined) return;
          event.preventDefault();
          if (row.kind === 'dir') toggleClosed(closed, row.path);
          else open = row.path;
          break;
        }
        default:
          break;
      }
    };
    addEventListener('keydown', onkey);
    return () => removeEventListener('keydown', onkey);
  });

  // A row the Tab key lands on becomes the keyboard's row, so the mark
  // follows real focus too.
  $effect(() => {
    const onfocus = (event: FocusEvent): void => {
      const target = event.target;
      if (!(target instanceof HTMLElement)) return;
      const row = target.closest('button.rv-dir, button.rv-frow');
      if (row instanceof HTMLElement && row.dataset.path !== undefined) {
        keyRow = row.dataset.path;
      }
    };
    addEventListener('focusin', onfocus);
    return () => removeEventListener('focusin', onfocus);
  });

  // Escape closes the open comment box, as its own hint says - and takes
  // the reader's target marks with it - and it closes an open reply the
  // same way.
  $effect(() => {
    if (comment.spot === null && reply.thread === null) return;
    const onkey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      if (comment.spot !== null) closeComposer();
      if (reply.thread !== null) {
        reply.thread = null;
        reply.draft = '';
      }
    };
    addEventListener('keydown', onkey);
    return () => removeEventListener('keydown', onkey);
  });

  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(tick);
  });

  // The reader re-mounts on a file change; the jump's line applies once its
  // handle is in hand.
  $effect(() => {
    const held = reader;
    const line = pendingLine;
    if (held === null || line === null) return;
    held.targetLine(line);
    pendingLine = null;
  });

  const sends: Sends = {
    write,
    dispatch: (command) => connection.dispatch(command as never, slot),
  };

  /**
   * One write, and the re-read that carries its outcome: a refusal is drawn
   * where the reader acted, and only a landed write settles the drafts - a
   * refused one keeps the words.
   */
  async function write(command: unknown): Promise<SendOutcome> {
    busy = true;
    try {
      const body = await connection.dispatch(command as never, slot);
      refusal = null;
      return { went: true, body };
    } catch (why) {
      refusal = why instanceof Error ? why.message : String(why);
      return { went: false, body: null };
    } finally {
      busy = false;
      // The reviews ride the record: a re-read is what carries a change.
      connection.refresh({ session: slot });
    }
  }

  /**
   * Put the composer away and take the reader's target with it: the tint,
   * the gutter `+` and the orange rule belong to the open box, and closing
   * one without the other left the marks behind (measured live, Ved
   * 2026-10-17).
   */
  function closeComposer(): void {
    comment.spot = null;
    comment.draft = '';
    reader?.targetLine(null);
  }

  /** A line was pressed in the reader: the composer opens on it. */
  function commentOn(line: number): void {
    if (open === null) return;
    comment.spot = { path: open, line };
    comment.draft = '';
  }

  async function commented(text: string): Promise<void> {
    const spot = comment.spot;
    const held = reader;
    if (spot === null || held === null) return;
    const lines = held.lines();
    const thread = newThread({
      path: spot.path,
      side: 'new',
      line: spot.line,
      text,
      context: contextForComment(lines, spot.line),
      lineText: lines[spot.line - 1] ?? '',
      baseRef: treeScope(slot),
      commit: null,
    });
    comment.busy = true;
    const { went } = await write(upsertCommand(slot, treeScope(slot), thread));
    comment.busy = false;
    if (!went) return;
    drafts.add(thread.id);
    closeComposer();
  }

  async function replied(thread: ReviewThreadRecord, text: string): Promise<void> {
    reply.busy = true;
    const { went } = await write(upsertCommand(slot, treeScope(slot), withReply(thread, text)));
    reply.busy = false;
    if (!went) return;
    drafts.add(thread.id);
    reply.thread = null;
    reply.draft = '';
  }

  async function resolved(thread: ReviewThreadRecord): Promise<void> {
    await write(statusCommand(slot, treeScope(slot), thread.id, 'Resolved'));
  }

  async function reopened(thread: ReviewThreadRecord): Promise<void> {
    await reopenThread(sends, slot, treeScope(slot), thread.id, latestRound(treeRounds, thread));
  }

  async function finished(summary: string | null): Promise<void> {
    if (drafts.size === 0) return;
    const sealed = await finishRound(
      sends,
      slot,
      treeScope(slot),
      [...drafts],
      summary,
      roundHasWork(treeThreads, drafts),
    );
    if (!sealed) return;
    drafts.clear();
    finishOpen = false;
  }

  /** A rail row: open its file and put the reader on its line. */
  function jump(row: (typeof rows)[number]): void {
    open = row.thread.anchor.path;
    pendingLine = row.line ?? row.thread.anchor.line;
  }

  const shown = $derived(matchedEntries(entries, query));
  const tree = $derived(indexTreeRows(shown, closed));

  /** Comments dropped on one file, from the seat-tree half - the row's pill. */
  function droppedCount(path: string): number {
    return treeThreads.filter((thread) => thread.anchor.path === path).length;
  }

  /**
   * The blocks drawn under the open file's lines: its threads, and the
   * composer when one is open. A blocked key carries the reply state, so
   * opening a reply row re-mounts its thread's card with the box in it.
   */
  const blocks = $derived.by(() => {
    const out: ReaderBlock[] = [];
    if (open === null) return out;
    for (const row of rows) {
      if (row.thread.anchor.path !== open || row.line === null) continue;
      const id = row.thread.id;
      out.push({
        line: row.line,
        key: `thread:${id}:${reply.thread === id ? 'reply' : 'shut'}`,
        mount: (host) => {
          const held = mount(Inline, {
            target: host,
            props: {
              row,
              rounds: treeRounds,
              get now() {
                return now;
              },
              get busy() {
                return busy;
              },
              get replying() {
                return reply.thread === id;
              },
              replyBox: reply,
              onrespond: () => {
                reply.thread = reply.thread === id ? null : id;
                reply.draft = '';
              },
              onreply: (text: string) => void replied(row.thread, text),
              onresolve: () => void resolved(row.thread),
              onreopen: () => void reopened(row.thread),
            },
          });
          return () => void unmount(held);
        },
      });
    }
    const spot = comment.spot;
    if (spot !== null && spot.path === open) {
      out.push({
        line: spot.line,
        key: `comment:${spot.path}:${spot.line}`,
        mount: (host) => {
          const held = mount(Box, {
            target: host,
            props: {
              class: 'rv-thread comment',
              headline: `${spot.path.split('/').pop() ?? ''} · line ${spot.line}`,
              placeholder: 'Comment on this line...',
              label: 'your comment',
              sendLabel: 'comment',
              box: comment,
              onsend: (text: string) => void commented(text),
              oncancel: closeComposer,
              field: (el: HTMLElement | null) => {
                // The caret goes in with the box (Ved, 2026-10-15).
                el?.focus();
              },
            },
          });
          return () => void unmount(held);
        },
      });
    }
    return out;
  });

  $effect(() => {
    const held = reader;
    if (held === null) return;
    held.setBlocks(blocks);
  });
</script>

<div class="files">
  <header class="files-top">
    <a class="files-back" href={hrefForSlot(slot)}>&lt;- session</a>
    <h1>Files</h1>
    {#if record !== null}
      <span class="files-facts">
        {record.state.scan_cwd}<span class="files-sep">|</span>{entries.length} files
      </span>
    {/if}
    {#if repliesToRead > 0}
      <span class="files-loop">{repliesToRead} to read</span>
    {/if}
    <!-- The narrow doors: real buttons at every width, hidden by the sheet
         rules while the columns are there (never hover-only). -->
    <span class="files-sheets">
      <button
        class="files-sheetbtn"
        type="button"
        aria-expanded={sheet === 'tree'}
        onclick={() => (sheet === 'tree' ? closeSheet() : openSheet('tree'))}>tree</button
      >
      <button
        class="files-sheetbtn"
        type="button"
        aria-expanded={sheet === 'comments'}
        onclick={() => (sheet === 'comments' ? closeSheet() : openSheet('comments'))}
        >comments ({rows.length})</button
      >
    </span>
  </header>

  {#if refusal !== null}
    <p class="files-said">{refusal}</p>
  {/if}

  {#if read.refused !== null}
    <main class="files-main"><p class="files-quiet">{read.refused}</p></main>
  {:else if record === null}
    <main class="files-main"><p class="files-quiet">Reading the seat...</p></main>
  {:else if entries.length === 0}
    <main class="files-main"><p class="files-quiet">The seat's tree has no files.</p></main>
  {:else}
    <div class="files-body">
      <div class="files-side" class:sheet={sheet === 'tree'}>
        <button class="files-sheetclose" type="button" onclick={closeSheet}>done</button>
        <input
          class="files-filter"
          type="text"
          placeholder="filter files"
          aria-label="filter files"
          data-editor="nowhere"
          bind:value={query}
        />
        {#if tree.length === 0}
          <p class="files-quiet">No file matches this filter.</p>
        {:else}
          <FileTree
            rows={tree}
            heading="Tree"
            label="the seat's tree"
            count={entries.length}
            {closed}
            here={open}
            active={keyRow}
            ondir={(path) => toggleClosed(closed, path)}
            onfile={(path) => (open = path)}
          >
            {#snippet trailing(row: TreeRow<unknown>)}
              {@const dropped = droppedCount(row.path)}
              {#if dropped > 0}
                <span class="rv-fcount"><span class="rv-ccnum">{dropped}</span></span>
              {/if}
            {/snippet}
          </FileTree>
        {/if}
      </div>
      <main class="files-main" aria-label="the file">
        {#if open === null}
          <p class="files-quiet">Pick a file from the tree.</p>
        {:else}
          <Reader {connection} {slot} path={open} oncomment={commentOn} bind:handle={reader} />
        {/if}
      </main>
      <div class="files-railhold" class:sheet={sheet === 'comments'}>
        <button class="files-sheetclose" type="button" onclick={closeSheet}>done</button>
        <Rail
          {rows}
          {repliesToRead}
          bind:finishOpen
          canFinish={drafts.size > 0}
          {busy}
          onjump={jump}
          onfinish={(summary) => void finished(summary)}
        />
      </div>
    </div>
  {/if}
</div>

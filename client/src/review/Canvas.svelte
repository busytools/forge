<script lang="ts">
  /**
   * The Files-changed half of the review page: one scope's diff, the file
   * tree, the threads drawn on their own lines, and the composer.
   *
   * **A scope is the All-changes view or one commit**, and a thread is drawn
   * only in the scope it was written in: a commit's diff is immutable, so an
   * anchor there is settled, while an all-changes anchor sits where the
   * server resolved it against the content this page draws.
   *
   * The viewer parses unified diff text and nothing else, so each file's
   * bounded hunks are re-emitted as text (`synthesizeFileDiff`); thread cards
   * and the composer are our own rows inserted after the anchored line's
   * row, which is what makes them span the table in both modes.
   */
  import { DiffView, DiffModeEnum } from '@git-diff-view/svelte';
  import { mount, unmount } from 'svelte';
  import type { ComponentProps } from 'svelte';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import { VList, type VListHandle } from 'virtua/svelte';
  import type { Connection } from '../socket';
  import type {
    DiffFileRecord,
    DiffLayerRecord,
    ReviewSetRecord,
    ReviewThreadRecord,
  } from '../session/wire';
  import { diffLayerFrom } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import {
    anchorRow,
    doorSpot,
    filesOf,
    findLine,
    insertRow,
    lineCounts,
    rowSide,
    synthesizeFileDiff,
    treeRows,
  } from './diff';
  import { canvasLine, statusName, threadsInScope } from './view';
  import { captureContext, type ComposedComment } from './write';
  import ThreadTurn from './ThreadTurn.svelte';

  type DiffFile = NonNullable<ComponentProps<typeof DiffView>['diffFile']>;

  let {
    connection,
    slot,
    scopeCommit,
    mode,
    layer,
    threads,
    reviews,
    busy,
    oncomment,
    onreply,
    onresolve,
    onreopen,
  }: {
    connection: Connection | null;
    slot: SessionSlot;
    /** The scope on screen: a commit's sha, or null for all changes. */
    scopeCommit: string | null;
    mode: 'unified' | 'split';
    /** The all-changes layer: the branch's own diff, the record's read. */
    layer: DiffLayerRecord;
    threads: ReviewThreadRecord[];
    reviews: ReviewSetRecord[];
    busy: boolean;
    /** Both answer whether the write landed: a refusal keeps the draft. */
    oncomment: (comment: ComposedComment) => Promise<boolean>;
    onreply: (thread: ReviewThreadRecord, text: string) => Promise<boolean>;
    onresolve: (thread: ReviewThreadRecord) => void;
    onreopen: (thread: ReviewThreadRecord) => void;
  } = $props();

  const closedDirs = new SvelteSet<string>();
  const closedFiles = new SvelteSet<string>();

  /** Commit scopes fetched through the socket, by sha. */
  const fetched = new SvelteMap<string, DiffLayerRecord>();

  function toggle(set: SvelteSet<string>, key: string): void {
    if (set.has(key)) set.delete(key);
    else set.add(key);
  }

  /** The composer's spot, and its draft. */
  let composing = $state<{ path: string; side: 'old' | 'new'; line: number } | null>(null);
  let composeText = $state('');
  /** The thread whose reply box is open, and its draft. */
  let replying = $state<string | null>(null);
  let replyText = $state('');

  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(tick);
  });

  /** A scope change closes the composer, whose spot belonged to the old scope. */
  $effect(() => {
    void scopeCommit;
    composing = null;
  });

  const scopeLayer = $derived(scopeCommit === null ? layer : (fetched.get(scopeCommit) ?? null));
  const scopeFiles = $derived(scopeLayer === null ? [] : filesOf(scopeLayer));
  const tree = $derived(treeRows(scopeFiles, closedDirs));
  const scoped = $derived(threadsInScope(threads, scopeCommit));

  /** The socket's own state, which is when a dropped ask is worth repeating. */
  let connState = $state<ReturnType<Connection['status']>>('connecting');
  $effect(() => {
    const open = connection;
    if (open === null) return;
    // Read once as well as subscribed: a page attaching to an already-open
    // socket hears no transition.
    connState = open.status();
    return open.onStatus((next) => (connState = next));
  });

  /**
   * One commit's diff, fetched when its scope is shown **and asked again on
   * every connection that opens**. `commitDiff` answers false when no socket
   * is open and no answer is coming, and nothing else says so - so the ask
   * rides the scope and the connection's own transitions rather than a
   * timer, which would keep asking a scope the server will never answer (an
   * error reply is not a frame) and would re-ask on top of a slow scan's
   * first answer.
   */
  $effect(() => {
    const sha = scopeCommit;
    const open = connection;
    void connState;
    if (sha === null || open === null || fetched.has(sha) || connState !== 'open') return;
    open.commitDiff(slot, sha);
  });

  $effect(() => {
    const open = connection;
    if (open === null) return;
    return open.onMessage((message) => {
      if (message.kind !== 'commit_diff') return;
      fetched.set(message.sha, diffLayerFrom(message.files));
    });
  });

  /**
   * The viewer's models, built per scope. The parser is heavy and only this
   * tab pays for it, so it is imported when the first scope is built.
   */
  interface BuiltFile {
    path: string;
    diffFile: DiffFile;
    file: DiffFileRecord;
  }
  let built = $state<BuiltFile[]>([]);
  /**
   * Which scope `built` was made for, **as a key rather than the layer
   * object**: a commit scope's layer comes out of a `SvelteMap`, and a
   * `===` between that value and one this file stored reactivates as a
   * proxy-vs-raw mismatch - the guard never holds, the effect rebuilds the
   * models on every pass, and each rebuild re-renders the mounted viewer's
   * whole table (measured: hundreds of thousands of mutation records per
   * two seconds in a commit scope, zero in all changes). A string compares
   * as itself.
   */
  let builtKey = $state<string | null>(null);
  $effect(() => {
    const scope = scopeLayer;
    const files = scopeFiles;
    const key = scopeCommit ?? 'all';
    if (scope === null || builtKey === key) return;
    void (async () => {
      const { DiffFile } = await import('@git-diff-view/core');
      const next = files.map((file) => {
        const instance = new DiffFile(
          file.oldPath ?? file.path,
          '',
          file.path,
          '',
          [synthesizeFileDiff(file)],
          '',
          '',
        );
        (instance as { init?: () => void }).init?.();
        return { path: file.path, diffFile: instance, file };
      });
      // A build that lands after the scope moved on is the old scope's.
      if ((scopeCommit ?? 'all') !== key) return;
      built = next;
      builtKey = key;
    })();
  });

  /** One canvas element per file. */
  let canvases = $state<Record<string, HTMLDivElement | void>>({});

  /**
   * **The canvas is virtualised over files, and that is what makes a mode
   * switch fluid.** Every mounted section carries a table the viewer
   * re-renders when the mode changes, so a branch of fifty files paid fifty
   * tables' worth of work per switch (measured: ~950ms, whichever way it
   * went). The DOM holds the sections on screen and the rest stays data -
   * the same bargain the chat's column strikes - and a switch now re-renders
   * the two or three tables a reader can actually see.
   */
  let list: VListHandle | null = $state(null);
  /**
   * Whether the page is in its narrow, one-column regime. The list exists
   * to bound what is mounted; a page that scrolls as one column has no
   * bounds to give it (its viewport inside an `overflow: visible` wrapper
   * measures nothing, and the list draws empty), so there the sections are
   * drawn plain.
   */
  let narrow = $state(false);
  $effect(() => {
    const query = window.matchMedia('(max-width: 980px)');
    const moved = (event: MediaQueryListEvent): void => {
      narrow = event.matches;
    };
    narrow = query.matches;
    query.addEventListener('change', moved);
    return () => query.removeEventListener('change', moved);
  });
  /**
   * The last measured height of each section, by path and mode: the
   * virtualiser drops a size when an item unmounts, and a section scrolled
   * back into view should not arrive at the estimator's guess first.
   */
  const heights = new SvelteMap<string, number>();
  const heightKey = (path: string): string => `${path}|${mode}`;

  function measureFile(node: HTMLElement, path: string): () => void {
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        if (entry.contentRect.height > 0) heights.set(heightKey(path), entry.contentRect.height);
      }
    });
    observer.observe(node);
    return () => observer.disconnect();
  }

  /** The tree's rows are the way to a file the list may not have drawn yet. */
  function jumpTo(path: string): void {
    const at = built.findIndex((file) => file.path === path);
    if (at < 0) return;
    // One scroller or the other: the virtualised list owns the wide layout,
    // and below 980 the page is the scroller and the section is its own
    // target - without this the rows were dead controls at that width.
    if (list !== null) list.scrollToIndex(at, { align: 'start' });
    else document.getElementById(path)?.scrollIntoView({ block: 'start' });
  }

  /**
   * A real button on every line's number, revealed under the pointer or on
   * focus - the composer's door, and one a keyboard and a finger both reach.
   *
   * **One door per cell in split, one per row in unified, each keyed by the
   * side it comments on**: an added or context line is the new side, a
   * removed line the old - the same rule the view this client replaced
   * applied to the same click, and what keeps a unified context line (whose
   * one gutter carries both numbers) from writing every comment old-side.
   *
   * Injected here because the viewer redraws its own rows; the button
   * carries a `data-for` key so a redraw leaves the one already there alone.
   */
  function injectDoors(el: HTMLDivElement, path: string): void {
    for (const tr of el.querySelectorAll('tr.diff-line')) {
      if (mode === 'split') {
        for (const side of ['new', 'old'] as const) {
          const spot = doorSpot(tr, side, mode);
          if (spot !== null) placeDoor(spot.cell, path, side, spot.line);
        }
        continue;
      }
      const hit = rowSide(tr, null, mode);
      const cell = tr.querySelector('td');
      if (hit === null || !(cell instanceof HTMLElement)) continue;
      placeDoor(cell, path, hit.side, hit.line);
    }
  }

  function placeDoor(cell: HTMLElement, path: string, side: 'old' | 'new', line: number): void {
    const key = `${side}:${line}`;
    if (cell.querySelector(`button.rv-add[data-for="${key}"]`) !== null) return;
    cell.classList.add('rv-add-host');
    const door = document.createElement('button');
    door.type = 'button';
    door.className = 'rv-add';
    door.dataset['for'] = key;
    door.textContent = '+';
    door.setAttribute('aria-label', `comment on the ${side} line ${line}`);
    door.addEventListener('click', () => {
      composing = { path, side, line };
      composeText = '';
    });
    cell.append(door);
  }

  /**
   * The thread cards and the composer, as rows inserted after the anchored
   * line's row.
   *
   * **A reconciling pass, not a rebuild.** The viewer re-renders its own
   * rows, so this runs on every mutation of a canvas - and the work it does
   * must reach a quiet steady state, or its own insertions re-arm the
   * observer and the page spins (measured: thousands of rebuilds a second
   * when a rebuild-all compared record order against positional order). So
   * each wanted row is checked into place: an entry already sitting after
   * its anchor is left untouched, one whose anchor row is not drawn (a
   * folded file, a scope mid-switch) does nothing at all, and only a real
   * insert, move or removal mutates the DOM.
   */
  $effect(() => {
    void scopeCommit;
    void mode;
    void closedFiles;
    void scoped;
    void composing;
    void replying;
    void busy;
    void now;
    const observers: MutationObserver[] = [];
    const draw = (el: HTMLDivElement, path: string) => {
      injectDoors(el, path);
      armClicks(el, path);
      const want: {
        key: string;
        side: 'old' | 'new';
        line: number;
        build: (at: HTMLTableRowElement) => HTMLTableRowElement;
      }[] = [];
      if (!closedFiles.has(path)) {
        for (const thread of scoped) {
          if (thread.anchor.path !== path) continue;
          const side = thread.anchor.side === 'old' ? 'old' : 'new';
          const placed = canvasLine(thread);
          if (placed === null) continue;
          const key = `t:${thread.id}:${statusName(thread.status)}:${thread.comments.length}:${
            replying === thread.id ? 'open' : ''
          }`;
          want.push({
            key,
            side,
            line: placed.line,
            build: (at: HTMLTableRowElement) => threadRow(at, thread, placed, side),
          });
        }
        if (composing !== null && composing.path === path) {
          want.push({
            key: `c:${composing.side}:${composing.line}`,
            side: composing.side,
            line: composing.line,
            build: (at: HTMLTableRowElement) => composerRow(at, path),
          });
        }
      }
      want.sort((a, b) => a.line - b.line || a.key.localeCompare(b.key));
      // The sheet's lint takes reactive collections for any mutable built-in,
      // and these reads run from a timer callback, where nothing tracks them.
      const existing = new SvelteMap<string, HTMLTableRowElement>();
      for (const row of el.querySelectorAll('tr.rv-insert')) {
        existing.set((row as HTMLElement).dataset['for'] ?? '', row as HTMLTableRowElement);
      }
      const kept = new SvelteSet<string>();
      for (const entry of want) {
        if (kept.has(entry.key)) continue;
        const at = anchorRow(el, entry.side, entry.line, mode);
        if (at === null) continue;
        const have = existing.get(entry.key);
        if (have !== undefined) {
          // Where it belongs is next after its anchor, other inserted rows
          // between the two forgiven: two threads on one line must not
          // reshuffle each other every pass.
          let prev = have.previousElementSibling;
          while (prev !== null && prev.classList.contains('rv-insert')) {
            prev = prev.previousElementSibling;
          }
          if (prev !== at) at.after(have);
          kept.add(entry.key);
          continue;
        }
        const row = entry.build(at);
        row.dataset['for'] = entry.key;
        at.after(row);
        kept.add(entry.key);
      }
      for (const [key, row] of existing) {
        if (kept.has(key)) continue;
        releaseRow(row);
        row.remove();
      }
    };
    for (const [path, el] of Object.entries(canvases)) {
      if (el === undefined || el === null) continue;
      requestAnimationFrame(() => draw(el, path));
      const observer = new MutationObserver(() => draw(el, path));
      observer.observe(el, { childList: true, subtree: true });
      observers.push(observer);
    }
    return () => {
      for (const observer of observers) observer.disconnect();
    };
  });

  /**
   * A click anywhere on a line row opens the composer there, the way the
   * number's `+` does - a pointer convenience beside the button a keyboard
   * and a finger already reach. A click that finished a text selection is a
   * reader copying code, not a comment.
   */
  function armClicks(el: HTMLDivElement, path: string): void {
    if (el.dataset['rvClicks'] === '1') return;
    el.dataset['rvClicks'] = '1';
    el.addEventListener('click', (event) => {
      const target = event.target;
      if (!(target instanceof HTMLElement)) return;
      if (target.closest('button, a, textarea, input') !== null) return;
      const selection = window.getSelection();
      if (selection !== null && String(selection).trim() !== '') return;
      const row = target.closest('tr.diff-line');
      if (row === null) return;
      const hit = rowSide(row, target, mode);
      if (hit === null) return;
      composing = { path, side: hit.side, line: hit.line };
      composeText = '';
    });
  }

  function span(cls: string, text: string): HTMLSpanElement {
    const node = document.createElement('span');
    node.className = cls;
    node.textContent = text;
    return node;
  }

  function mark(status: string): HTMLSpanElement {
    const node = document.createElement('span');
    node.className = `rv-mk ${status}`;
    node.setAttribute('aria-hidden', 'true');
    return node;
  }

  /**
   * The Svelte roots a built row owns, so a row the reconciler drops lets
   * them go with it. Keyed by the element the root was mounted into.
   */
  const roots = new WeakMap<Element, () => void>();

  /** One turn, drawn by the piece both halves of the page draw. */
  function turnBlock(turn: ReviewThreadRecord['comments'][number], index: number): HTMLElement {
    const host = document.createElement('div');
    const held = mount(ThreadTurn, {
      target: host,
      props: { turn, rounds: reviews, index, now },
    });
    roots.set(host, () => void unmount(held));
    return host;
  }

  /** Let go of every root a row's subtree owns, before the row is dropped. */
  function releaseRow(row: Element): void {
    for (const el of row.querySelectorAll('*')) {
      const letGo = roots.get(el);
      if (letGo === undefined) continue;
      letGo();
      roots.delete(el);
    }
  }

  function threadCard(thread: ReviewThreadRecord, movedFrom: number | null): HTMLElement {
    const card = document.createElement('article');
    card.className = 'rv-thread compact';
    const tops = document.createElement('div');
    tops.className = 'rv-tops';
    tops.append(mark(statusName(thread.status)));
    tops.append(span(`rv-st ${statusName(thread.status)}`, statusName(thread.status)));
    tops.append(
      span('rv-where', `${thread.anchor.path.split('/').pop() ?? ''} · line ${thread.anchor.line}`),
    );
    if (movedFrom !== null) tops.append(span('rv-moved', `moved from ${movedFrom}`));
    card.append(tops);
    thread.comments.forEach((turn, index) => card.append(turnBlock(turn, index)));
    const acts = document.createElement('div');
    acts.className = 'rv-acts';
    const respond = document.createElement('button');
    respond.type = 'button';
    respond.className = 'rv-a';
    respond.textContent = 'respond';
    respond.disabled = busy;
    respond.addEventListener('click', () => {
      replying = replying === thread.id ? null : thread.id;
      replyText = '';
    });
    acts.append(respond);
    if (statusName(thread.status) === 'resolved') {
      const reopen = document.createElement('button');
      reopen.type = 'button';
      reopen.className = 'rv-a';
      reopen.textContent = 'reopen';
      reopen.disabled = busy;
      reopen.addEventListener('click', () => void onreopen(thread));
      acts.append(reopen);
    } else {
      const resolve = document.createElement('button');
      resolve.type = 'button';
      resolve.className = 'rv-a';
      resolve.textContent = 'resolve';
      resolve.disabled = busy;
      resolve.addEventListener('click', () => void onresolve(thread));
      acts.append(resolve);
    }
    card.append(acts);
    if (replying === thread.id) {
      const box = document.createElement('div');
      box.className = 'rv-reply';
      const area = document.createElement('textarea');
      area.className = 'rv-txarea';
      area.rows = 2;
      area.value = replyText;
      area.placeholder = 'Respond to this thread...';
      area.setAttribute('aria-label', 'your response');
      area.dataset['editor'] = 'nowhere';
      box.append(area);
      const row = document.createElement('div');
      row.className = 'rv-acts';
      const send = document.createElement('button');
      send.type = 'button';
      send.className = 'rv-a on';
      send.textContent = 'send';
      send.disabled = busy || replyText.trim() === '';
      area.addEventListener('input', () => {
        replyText = area.value;
        // Set at build AND on every keystroke: a row built once carries the
        // state it was built in, so a one-time `disabled` would leave the
        // button dead however much was typed into it.
        send.disabled = busy || area.value.trim() === '';
      });
      send.addEventListener('click', () => {
        // The box closes only once the write landed: a refusal keeps the
        // reader's words where they wrote them.
        void onreply(thread, replyText.trim()).then((filed) => {
          if (!filed) return;
          replying = null;
          replyText = '';
        });
      });
      const cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.className = 'rv-a';
      cancel.textContent = 'cancel';
      cancel.addEventListener('click', () => {
        replying = null;
        replyText = '';
      });
      row.append(send, cancel);
      box.append(row);
      card.append(box);
    }
    return card;
  }

  function threadRow(
    at: HTMLTableRowElement,
    thread: ReviewThreadRecord,
    placed: { line: number; movedFrom: number | null },
    side: 'old' | 'new',
  ): HTMLTableRowElement {
    const wrap = document.createElement('div');
    wrap.className = 'rv-inline';
    if (statusName(thread.status) === 'resolved') {
      const marker = document.createElement('div');
      marker.className = 'rv-marker';
      marker.append(mark('resolved'));
      marker.append(span('rv-mk-word', 'resolved'));
      marker.append(span('rv-mk-first', thread.comments[0]?.text ?? ''));
      wrap.append(marker);
    } else {
      wrap.append(threadCard(thread, placed.movedFrom));
    }
    return insertRow(at, side, mode, wrap);
  }

  function composerRow(at: HTMLTableRowElement, path: string): HTMLTableRowElement {
    const wrap = document.createElement('div');
    wrap.className = 'rv-inline';
    const card = document.createElement('article');
    card.className = 'rv-thread compact';
    const tops = document.createElement('div');
    tops.className = 'rv-tops';
    tops.append(span('rv-where', `${path} · line ${composing?.line ?? 0}`));
    card.append(tops);
    const area = document.createElement('textarea');
    area.className = 'rv-txarea';
    area.rows = 3;
    area.value = composeText;
    area.placeholder = 'Comment on this line...';
    area.setAttribute('aria-label', 'your comment');
    area.dataset['editor'] = 'nowhere';
    card.append(area);
    const acts = document.createElement('div');
    acts.className = 'rv-acts';
    const comment = document.createElement('button');
    comment.type = 'button';
    comment.className = 'rv-a on';
    comment.textContent = 'comment';
    comment.disabled = busy || composeText.trim() === '';
    area.addEventListener('input', () => {
      composeText = area.value;
      // As in the reply box: the row is built once, so the button's state is
      // kept current here rather than only where it was created.
      comment.disabled = busy || area.value.trim() === '';
    });
    comment.addEventListener('click', () => {
      const at = composing;
      if (at === null) return;
      const file = scopeFiles.find((held) => held.path === at.path);
      const found = file === undefined ? null : findLine(file, at.side, at.line);
      if (found === null) return;
      const line = found.hunk.lines[found.index];
      if (line === undefined) return;
      // As with a reply: the composer stays open, holding the words, until
      // the write lands.
      void oncomment({
        path: at.path,
        side: at.side,
        line: at.line,
        text: composeText.trim(),
        context: captureContext(found.hunk.lines, found.index),
        lineText: line.text,
      }).then((filed) => {
        if (!filed) return;
        composing = null;
        composeText = '';
      });
    });
    const cancel = document.createElement('button');
    cancel.type = 'button';
    cancel.className = 'rv-a';
    cancel.textContent = 'cancel';
    cancel.addEventListener('click', () => {
      composing = null;
      composeText = '';
    });
    acts.append(comment, cancel);
    card.append(acts);
    wrap.append(card);
    return insertRow(at, composing?.side ?? 'new', mode, wrap);
  }
</script>

<nav class="rv-files" aria-label="files in this scope">
  <h2 class="rv-h">Files <span class="rv-n">{scopeFiles.length}</span></h2>
  {#each tree as row (row.kind + ':' + row.path)}
    {#if row.kind === 'dir'}
      <button
        type="button"
        class="rv-dir"
        style="--d:{row.depth}"
        aria-expanded={!closedDirs.has(row.path)}
        onclick={() => toggle(closedDirs, row.path)}
      >
        <span class="rv-arw" class:open={!closedDirs.has(row.path)}>&#9656;</span>
        <span class="rv-dirname">{row.name}/</span>
        <span class="rv-dircount">{row.count}</span>
      </button>
    {:else}
      {@const file = row.file}
      {#if file !== null}
        {@const at = lineCounts(file)}
        {@const mine = scoped.filter(
          (thread) => thread.anchor.path === file.path && canvasLine(thread) !== null,
        )}
        <button
          type="button"
          class="rv-frow"
          class:renamed={file.oldPath !== null}
          style="--d:{row.depth}"
          title={row.path}
          onclick={() => jumpTo(file.path)}
        >
          <span class="rv-fpath">{row.name}</span>
          <span class="rv-fstat">
            {#if at.plus > 0}<span class="rv-plus">+{at.plus}</span>{/if}
            {#if at.minus > 0}<span class="rv-minus">-{at.minus}</span>{/if}
          </span>
          <span class="rv-fcount">
            {#if mine.length}<span class="rv-ccnum">{mine.length}</span>{/if}
          </span>
        </button>
      {/if}
    {/if}
  {/each}
</nav>

<main class="rv-canvas">
  {#if scopeLayer === null}
    <p class="rv-quiet">Reading the commit...</p>
  {:else if scopeLayer.state === 'scan_failed'}
    <p class="rv-quiet">This scope's diff could not be read.</p>
  {:else if scopeLayer.state === 'unknown'}
    <p class="rv-quiet">
      This scope's read answered {scopeLayer.raw}, which this build cannot name.
    </p>
  {:else if scopeFiles.length === 0}
    <p class="rv-quiet">
      {scopeCommit === null
        ? 'This branch has no changes against its base.'
        : 'This commit changed nothing.'}
    </p>
  {:else if narrow}
    {#each built as file (file.path)}
      {@render fileSection(file)}
    {/each}
  {:else}
    <VList
      class="rv-canvas-list"
      data={built}
      getKey={(file: BuiltFile) => file.path}
      bind:this={list}
    >
      {#snippet children(file: BuiltFile)}
        {@render fileSection(file)}
      {/snippet}
    </VList>
  {/if}
</main>

{#snippet fileSection(file: BuiltFile)}
  {@const mine = scoped.filter(
    (thread) => thread.anchor.path === file.path && canvasLine(thread) !== null,
  )}
  <!-- The floor is the section's, and the MEASUREMENT is its inner body's:
       an observer on the floored section records the clamp, so the height
       can never shrink and a collapsed file leaves a blank as tall as its
       diff (measured: a section's rect and min-height both 7887px after the
       collapse, its .rv-diff gone). -->
  <section
    class="rv-file"
    id={file.path}
    style:min-height={heights.get(heightKey(file.path)) === undefined
      ? undefined
      : `${heights.get(heightKey(file.path))}px`}
  >
    <div class="rv-file-body" {@attach (node: HTMLElement) => measureFile(node, file.path)}>
      <div class="rv-filehead">
        <button
          type="button"
          class="rv-fold"
          aria-expanded={!closedFiles.has(file.path)}
          aria-label="collapse this file"
          onclick={() => toggle(closedFiles, file.path)}
        >
          <span class="rv-arw" class:open={!closedFiles.has(file.path)}>&#9656;</span>
        </button>
        <span class="rv-path">
          {#if file.file.oldPath !== null}<span class="rv-from"
              >{file.file.oldPath} &#8594;
            </span>{/if}{file.path}
        </span>
        <span class="rv-tally">
          {#if mine.length}
            <span class="rv-cc">{mine.length} {mine.length === 1 ? 'thread' : 'threads'}</span>
          {/if}
          {#if file.file.truncated}<span class="rv-cc">truncated</span>{/if}
          {#if file.file.binary}<span class="rv-cc">binary</span>{/if}
        </span>
      </div>
      {#if !closedFiles.has(file.path)}
        <div class="rv-diff" data-rv-path={file.path} bind:this={canvases[file.path]}>
          <DiffView
            diffFile={file.diffFile}
            diffViewMode={mode === 'unified' ? DiffModeEnum.Unified : DiffModeEnum.Split}
            diffViewTheme="dark"
            diffViewHighlight={true}
            diffViewFontSize={12}
            diffViewWrap={true}
          />
        </div>
      {/if}
    </div>
  </section>
{/snippet}

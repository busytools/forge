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
  import { filesOf, findLine, lineCounts, synthesizeFileDiff, treeOf } from './diff';
  import { ageOf, authorName, canvasLine, statusName, threadsInScope, turnRound } from './view';
  import { captureContext, type ComposedComment } from './write';

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
    oncomment: (comment: ComposedComment) => void;
    onreply: (thread: ReviewThreadRecord, text: string) => void;
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
  const tree = $derived(treeOf(scopeFiles));
  const scoped = $derived(threadsInScope(threads, scopeCommit));

  /** One commit's diff, fetched when its scope is shown for the first time. */
  $effect(() => {
    const sha = scopeCommit;
    const open = connection;
    if (sha === null || open === null || fetched.has(sha)) return;
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
  let builtLayer = $state<DiffLayerRecord | null>(null);
  $effect(() => {
    const scope = scopeLayer;
    const files = scopeFiles;
    if (scope === null || builtLayer === scope) return;
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
      built = next;
      builtLayer = scope;
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
    if (at >= 0) list?.scrollToIndex(at, { align: 'start' });
  }

  /**
   * A real button on every line's number, revealed under the pointer or on
   * focus - the composer's door, and one a keyboard and a finger both reach.
   *
   * **One door per cell in split, one per row in unified, each keyed by the
   * side it comments on**: an added or context line is the new side, a
   * removed line the old - the terminal's own rule for the same click, and
   * what keeps a unified context line (whose one gutter carries both
   * numbers) from writing every comment old-side.
   *
   * Injected here because the viewer redraws its own rows; the button
   * carries a `data-for` key so a redraw leaves the one already there alone.
   */
  function injectDoors(el: HTMLDivElement, path: string): void {
    if (mode === 'split') {
      const spots = [
        ['new', 'td[data-side="new"] span[data-line-num], tr[data-side="new"] span[data-line-num]'],
        ['old', 'td[data-side="old"] span[data-line-num], tr[data-side="old"] span[data-line-num]'],
      ] as const;
      for (const tr of el.querySelectorAll('tr.diff-line')) {
        for (const [side, selector] of spots) {
          const span = tr.querySelector(selector);
          if (span === null || !(span instanceof HTMLElement)) continue;
          const cell = span.closest('td') ?? span.parentElement;
          if (cell === null) continue;
          const line = lineOf(span);
          if (line === null) continue;
          placeDoor(cell, path, side, line);
        }
      }
      return;
    }
    for (const tr of el.querySelectorAll('tr.diff-line')) {
      const hit = rowSide(tr, null);
      if (hit === null) continue;
      const cell = tr.querySelector('td');
      if (cell === null) continue;
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

  /** The line a number span names, whichever attribute form carries it. */
  function lineOf(span: HTMLElement): number | null {
    const line = Number(
      span.dataset['lineNewNum'] ?? span.dataset['lineOldNum'] ?? span.textContent,
    );
    return Number.isInteger(line) && line > 0 ? line : null;
  }

  /**
   * The side and line a line-row click means: the clicked cell's own side in
   * split, and in unified the new number where the row has one - a removed
   * line has only the old.
   */
  function rowSide(
    row: Element,
    target: HTMLElement | null,
  ): { side: 'old' | 'new'; line: number } | null {
    const numOf = (side: 'old' | 'new'): number | null => {
      const selector =
        mode === 'unified'
          ? `span[data-line-${side}-num]`
          : `td[data-side="${side}"] span[data-line-num], tr[data-side="${side}"] span[data-line-num]`;
      const span = row.querySelector(selector);
      return span instanceof HTMLElement ? lineOf(span) : null;
    };
    const at = (side: 'old' | 'new'): { side: 'old' | 'new'; line: number } | null => {
      const line = numOf(side);
      return line === null ? null : { side, line };
    };
    const clicked =
      mode === 'split' ? target?.closest('td[data-side]')?.getAttribute('data-side') : null;
    if (clicked === 'old') return at('old') ?? at('new');
    return at('new') ?? at('old');
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
        const at = anchorRow(el, entry.side, entry.line);
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
        if (!kept.has(key)) row.remove();
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
      const hit = rowSide(row, target);
      if (hit === null) return;
      composing = { path, side: hit.side, line: hit.line };
      composeText = '';
    });
  }

  /** The row a line's number sits on, by the side that number belongs to. */
  function anchorRow(
    el: HTMLElement,
    side: 'old' | 'new',
    line: number,
  ): HTMLTableRowElement | null {
    const anchor =
      mode === 'unified'
        ? el.querySelector(`span[data-line-${side}-num="${line}"]`)
        : (el.querySelector(`td[data-side="${side}"] span[data-line-num="${line}"]`) ??
          el.querySelector(`tr[data-side="${side}"] span[data-line-num="${line}"]`));
    return anchor?.closest('tr') ?? null;
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
   * One inserted row, shaped after the row it lands under. **In split the
   * card holds the half it comments on** - a right-side comment under a
   * right-side line, a left-side one under the left - and in unified it
   * takes the whole lane, where the line itself does.
   *
   * The anchor's own cells decide the geometry rather than a fixed count:
   * a split row pads its missing half with a spanning placeholder cell, so
   * rows in one file do not all carry the same number of cells.
   */
  function insertRow(
    at: HTMLTableRowElement,
    side: 'old' | 'new',
    body: HTMLElement,
  ): HTMLTableRowElement {
    const row = document.createElement('tr');
    row.className = 'rv-insert';
    const cells = [...at.children].map((cell) => {
      const el = cell as HTMLElement;
      const own =
        el.dataset['side'] ??
        (el.className.includes('old') ? 'old' : el.className.includes('new') ? 'new' : null);
      return { side: own, colspan: Number(el.getAttribute('colspan') ?? 1) };
    });
    const card = (colspan: number): HTMLTableCellElement => {
      const cell = document.createElement('td');
      cell.className = 'rv-insert-cell';
      cell.colSpan = colspan;
      return cell;
    };
    const spanOf = (which: 'old' | 'new'): number =>
      cells.filter((cell) => cell.side === which).reduce((n, cell) => n + cell.colspan, 0);
    // No cell names a side: unified, whose line owns the whole width.
    if (!cells.some((cell) => cell.side !== null)) {
      const cell = card(cells.reduce((n, held) => n + held.colspan, 0));
      cell.append(body);
      row.append(cell);
      return row;
    }
    const mine = card(spanOf(side));
    mine.append(body);
    const theirs = card(spanOf(side === 'new' ? 'old' : 'new'));
    if (side === 'new') row.append(theirs, mine);
    else row.append(mine, theirs);
    return row;
  }

  function turnBlock(turn: ReviewThreadRecord['comments'][number], index: number): HTMLElement {
    const block = document.createElement('div');
    block.className = turn.author.kind === 'user' ? 'rv-turn you' : 'rv-turn';
    const who = document.createElement('div');
    who.className = 'rv-who';
    who.append(span('rv-nm', authorName(turn.author)));
    const round = turnRound(reviews, turn);
    if (round !== null) who.append(span('rv-at', `R${round}`));
    else if (index > 0) who.append(span('rv-at', ageOf(turn.at, now)));
    block.append(who);
    const text = document.createElement('p');
    text.className = 'rv-tx';
    text.textContent = turn.text;
    block.append(text);
    return block;
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
      reopen.addEventListener('click', () => onreopen(thread));
      acts.append(reopen);
    } else {
      const resolve = document.createElement('button');
      resolve.type = 'button';
      resolve.className = 'rv-a';
      resolve.textContent = 'resolve';
      resolve.disabled = busy;
      resolve.addEventListener('click', () => onresolve(thread));
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
        onreply(thread, replyText.trim());
        replying = null;
        replyText = '';
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
    return insertRow(at, side, wrap);
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
      oncomment({
        path: at.path,
        side: at.side,
        line: at.line,
        text: composeText.trim(),
        context: captureContext(found.hunk.lines, found.index),
        lineText: line.text,
      });
      composing = null;
      composeText = '';
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
    return insertRow(at, composing?.side ?? 'new', wrap);
  }
</script>

<nav class="rv-files" aria-label="files in this scope">
  <h2 class="rv-h">Files <span class="rv-n">{scopeFiles.length}</span></h2>
  {#each tree as [dir, files] (dir)}
    {#if dir}
      <button type="button" class="rv-dir" onclick={() => toggle(closedDirs, dir)}>
        <span class="rv-arw" class:open={!closedDirs.has(dir)}>&#9656;</span>
        <span class="rv-dirpath">{dir}/</span>
        <span class="rv-dircount">{files.length}</span>
      </button>
    {/if}
    {#if !dir || !closedDirs.has(dir)}
      {#each files as file (file.path)}
        {@const at = lineCounts(file)}
        {@const mine = scoped.filter(
          (thread) => thread.anchor.path === file.path && canvasLine(thread) !== null,
        )}
        <button
          type="button"
          class="rv-frow"
          class:renamed={file.oldPath !== null}
          onclick={() => jumpTo(file.path)}
        >
          <span class="rv-fpath">{file.path.split('/').pop()}</span>
          <span class="rv-fstat">
            <span class="rv-plus">+{at.plus}</span>
            <span class="rv-minus">-{at.minus}</span>
          </span>
          <span class="rv-fcount">
            {#if mine.length}<span class="rv-ccnum">{mine.length}</span>{/if}
          </span>
        </button>
      {/each}
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
  {:else}
    <VList
      class="rv-canvas-list"
      data={built}
      getKey={(file: BuiltFile) => file.path}
      bind:this={list}
    >
      {#snippet children(file: BuiltFile)}
        {@const mine = scoped.filter(
          (thread) => thread.anchor.path === file.path && canvasLine(thread) !== null,
        )}
        <section
          class="rv-file"
          id={file.path}
          style:min-height={heights.get(heightKey(file.path)) === undefined
            ? undefined
            : `${heights.get(heightKey(file.path))}px`}
          {@attach (node: HTMLElement) => measureFile(node, file.path)}
        >
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
        </section>
      {/snippet}
    </VList>
  {/if}
</main>

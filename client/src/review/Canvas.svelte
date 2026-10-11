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
  let built = $state<{ path: string; diffFile: DiffFile; file: DiffFileRecord }[]>([]);
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
   * A real button on every line's number cell, revealed under the pointer or
   * on focus - the composer's door, and one a keyboard and a finger both
   * reach. Injected here because the viewer redraws its own rows; the button
   * carries a `data-for` key so a redraw leaves the one already there alone.
   */
  function injectAddButtons(el: HTMLDivElement, path: string): void {
    const spots = [
      ['new', 'span[data-line-new-num], td[data-side="new"] span[data-line-num]'],
      ['old', 'span[data-line-old-num], td[data-side="old"] span[data-line-num]'],
    ] as const;
    for (const tr of el.querySelectorAll('tr.diff-line')) {
      for (const [side, selector] of spots) {
        const span = tr.querySelector(selector);
        if (span === null || !(span instanceof HTMLElement)) continue;
        const cell = span.closest('td') ?? span.parentElement;
        if (cell === null) continue;
        const line = Number(
          span.dataset['lineNewNum'] ?? span.dataset['lineOldNum'] ?? span.textContent,
        );
        if (!Number.isInteger(line) || line <= 0) continue;
        const key = `${side}:${line}`;
        if (cell.querySelector(`button.rv-add[data-for="${key}"]`) !== null) continue;
        cell.classList.add('rv-add-host');
        const door = document.createElement('button');
        door.type = 'button';
        door.className = 'rv-add';
        door.dataset['for'] = key;
        door.setAttribute('aria-label', `comment on line ${line}`);
        door.addEventListener('click', () => {
          composing = { path, side, line };
          composeText = '';
        });
        cell.append(door);
      }
    }
  }

  /**
   * The thread cards and the composer, as rows inserted after the anchored
   * line's row.
   *
   * The viewer re-renders its own rows (a mode switch, a fold), so this runs
   * on every mutation of a canvas and is idempotent by each row's dataset
   * key: rows it already drew are left alone rather than rebuilt.
   */
  $effect(() => {
    void scopeCommit;
    void mode;
    void closedFiles;
    void scoped;
    void composing;
    void replying;
    void now;
    const observers: MutationObserver[] = [];
    const draw = (el: HTMLDivElement, path: string) => {
      injectAddButtons(el, path);
      const want: { key: string; line: number; build: () => HTMLTableRowElement }[] = [];
      if (!closedFiles.has(path)) {
        for (const thread of scoped) {
          if (thread.anchor.path !== path || thread.anchor.side !== 'new') continue;
          const placed = canvasLine(thread);
          if (placed === null) continue;
          const key = `t:${thread.id}:${statusName(thread.status)}:${thread.comments.length}:${
            replying === thread.id ? 'open' : ''
          }`;
          want.push({ key, line: placed.line, build: () => threadRow(thread, placed, path) });
        }
        if (composing !== null && composing.path === path) {
          want.push({
            key: `c:${composing.side}:${composing.line}`,
            line: composing.line,
            build: () => composerRow(path),
          });
        }
      }
      const have = [...el.querySelectorAll('tr.rv-insert')];
      const keys = have.map((row) => (row as HTMLElement).dataset['for'] ?? '');
      if (keys.length === want.length && want.every((entry, i) => entry.key === keys[i])) return;
      for (const row of have) row.remove();
      for (const entry of want) {
        const at = anchorRow(el, entry.line);
        if (at === null) continue;
        const row = entry.build();
        row.dataset['for'] = entry.key;
        at.after(row);
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

  /** The row a line's number sits on, in whichever mode drew it. */
  function anchorRow(el: HTMLElement, line: number): HTMLTableRowElement | null {
    const anchor =
      el.querySelector(`span[data-line-new-num="${line}"]`) ??
      el.querySelector(`td[data-side="new"] span[data-line-num="${line}"]`) ??
      el.querySelector(`tr[data-side="new"] span[data-line-num="${line}"]`);
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

  /** One row spanning the table, with `body` inside its single cell. */
  function insertRow(el: HTMLDivElement, body: HTMLElement): HTMLTableRowElement {
    const row = document.createElement('tr');
    row.className = 'rv-insert';
    const cell = document.createElement('td');
    cell.colSpan = el.querySelectorAll('colgroup col').length || 4;
    cell.className = 'rv-insert-cell';
    row.append(cell);
    cell.append(body);
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
    thread: ReviewThreadRecord,
    placed: { line: number; movedFrom: number | null },
    path: string,
  ): HTMLTableRowElement {
    const host = canvases[path];
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
    if (host === undefined || host === null) {
      const row = document.createElement('tr');
      row.className = 'rv-insert';
      row.append(wrap);
      return row;
    }
    return insertRow(host, wrap);
  }

  function composerRow(path: string): HTMLTableRowElement {
    const host = canvases[path];
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
    if (host === undefined || host === null) {
      const row = document.createElement('tr');
      row.className = 'rv-insert';
      row.append(wrap);
      return row;
    }
    return insertRow(host, wrap);
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
        <a class="rv-frow" href={'#' + file.path} class:renamed={file.oldPath !== null}>
          <span class="rv-fpath">{file.path.split('/').pop()}</span>
          <span class="rv-fstat">
            <span class="rv-plus">+{at.plus}</span>
            <span class="rv-minus">-{at.minus}</span>
          </span>
          <span class="rv-fcount">
            {#if mine.length}<span class="rv-ccnum">{mine.length}</span>{/if}
          </span>
        </a>
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
  {/if}
  {#each built as file (file.path)}
    {@const mine = scoped.filter(
      (thread) => thread.anchor.path === file.path && canvasLine(thread) !== null,
    )}
    <section class="rv-file" id={file.path}>
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
  {/each}
</main>

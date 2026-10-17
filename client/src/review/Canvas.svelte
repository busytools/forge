<script lang="ts">
  /**
   * The Files-changed half of the review page: one scope's diff, the file
   * tree, the threads drawn on their own lines, and the composer.
   *
   * **A scope is the All-changes view or one commit.** A commit's own view
   * draws its threads at their recorded lines - a commit's diff is immutable,
   * so an anchor there is settled - while the all-changes view draws every
   * thread the server resolved against the content this page draws, a
   * commit-scoped one included when the branch still carries its line. A
   * commit's view draws none of the branch's own threads: the record holds no
   * landing for them in that numbering.
   *
   * The viewer parses unified diff text and nothing else, so each file's
   * bounded hunks are re-emitted as text (`synthesizeFileDiff`); thread cards
   * and the composer are our own rows inserted after the anchored line's
   * row, which is what makes them span the table in both modes.
   */
  import { DiffView, DiffModeEnum } from '@git-diff-view/svelte';
  import { mount, unmount, untrack, type ComponentProps } from 'svelte';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import { VList, type VListHandle } from 'virtua/svelte';
  import FileTree from '../components/FileTree.svelte';
  import { toggleClosed, type TreeRow } from '../components/tree';
  import type { Dictation } from '../composer/dictate.svelte';
  import type { Connection } from '../socket';
  import type { CommentBox, ReplyBox, Voice } from './boxes';
  import type {
    DiffFileRecord,
    DiffLayerRecord,
    FileContentRecord,
    ReviewSetRecord,
    ReviewThreadRecord,
  } from '../session/wire';
  import { diffLayerFrom, fileContentFrom } from '../session/wire';
  import { subjectKey } from '../protocol';
  import type { SessionSlot } from '../wire/types';
  import {
    anchorRow,
    currentFileAt,
    doorSpot,
    filesOf,
    findLine,
    GAP_SMALL,
    GAP_STEP,
    gapRemaining,
    gapsOf,
    insertRow,
    lineCounts,
    rowSide,
    synthesizeFileDiff,
    treeRows,
    type FileLines,
    type Gap,
    type GapReveal,
  } from './diff';
  import { canvasLine, statusName, threadsInScope } from './view';
  import { captureContext, type ComposedComment, type ReviewSpot } from './write';
  import { Mounts } from './mounts';
  import ThreadTurn from './ThreadTurn.svelte';
  import WordsBox from './WordsBox.svelte';

  type DiffFile = NonNullable<ComponentProps<typeof DiffView>['diffFile']>;

  /** Which way a gap's strip opens it: the two ends, or the whole remainder. */
  type GapWay = 'all' | 'down' | 'up';

  let {
    connection,
    slot,
    scopeCommit,
    mode,
    layer,
    threads,
    reviews,
    busy,
    dictate,
    keysOff = false,
    focusComment,
    box = $bindable(),
    reply = $bindable(),
    voice,
    onpress,
    onhold,
    onelement,
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
    /** The take this page owns, which every box's mic drives. */
    dictate: Dictation;
    /** Hold the tree's keys: the page's picker takes the arrows while it is
     *  open, and two walkers on one key are one too many. */
    keysOff?: boolean;
    /** The comment the stepper moved to, and the step's own number: the
     *  canvas brings that card into view, a repeat of the same target
     *  included. */
    focusComment: { path: string; side: 'old' | 'new'; line: number; seq: number } | null;
    /** The comment box this canvas draws: the spot, the words, the write in
     *  flight, the take's line, and where its take is. The page holds it,
     *  because the words and the take outlive the row the canvas builds. */
    box: CommentBox;
    /** The reply box under a thread, which either half of the page draws. */
    reply: ReplyBox;
    /** What dictation means to this page, shared by every box it draws. */
    voice: Voice;
    /** The reader pressed a box's mic, or began a take from the key in it. */
    onpress: (at: 'comment' | 'reply') => void;
    /** A box's mic was held: the settings open off that box. */
    onhold: (at: 'comment' | 'reply') => void;
    /** One box's field element, so a landing or a panel close can focus it. */
    onelement: (at: 'comment' | 'reply', el: HTMLElement | null) => void;
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
    toggleClosed(set, key);
  }

  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(tick);
  });

  /** A scope change closes the composer, whose spot belonged to the old scope. */
  $effect(() => {
    void scopeCommit;
    closeBox();
  });

  /**
   * **A box goes with the row that drew it.** Folding the file, or a scope
   * that drops the thread, releases the row while the box's own state stayed
   * set - a take, or a key press, would then target a box nothing draws, and
   * the words would land where the reader cannot see them.
   */
  $effect(() => {
    if (box.spot !== null && closedFiles.has(box.spot.path)) closeBox();
    // A thread the RECORD no longer holds: the reply box has nothing left to
    // open on. A thread merely out of the current SCOPE keeps its state -
    // the conversation half may draw it, and a typed draft is not the
    // scope's to discard.
    if (reply.thread !== null && !threads.some((thread) => thread.id === reply.thread)) {
      reply.thread = null;
      reply.draft = '';
    }
  });

  const scopeLayer = $derived(scopeCommit === null ? layer : (fetched.get(scopeCommit) ?? null));
  const scopeFiles = $derived(scopeLayer === null ? [] : filesOf(scopeLayer));
  const tree = $derived(treeRows(scopeFiles, closedDirs));
  /** One file by its path, so the tree's trailing snippet looks up what it
   *  draws in constant time rather than scanning the scope per row. */
  const fileByPath = $derived(new Map(scopeFiles.map((file) => [file.path, file])));
  const scoped = $derived(threadsInScope(threads, scopeCommit));

  /** The socket's own state, which is when a dropped ask is worth repeating. */
  let connState = $state<ReturnType<Connection['status']>>('connecting');
  $effect(() => {
    const open = connection;
    if (open === null) return;
    // Read once as well as subscribed: a page attaching to an already-open
    // socket hears no transition.
    connState = open.status();
    return open.onStatus((next) => {
      // **An ask the connection did not carry is not remembered.** A socket
      // that goes down mid-ask never answers it, so its key comes off the
      // asked set now and the scope asks again when the connection is back -
      // the same rule the commit scope's own fetch keeps.
      if (next !== 'open') askedFrom.clear();
      connState = next;
    });
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
    const seat = subjectKey({ session: slot });
    return open.onMessage((message) => {
      if (message.kind === 'commit_diff') {
        fetched.set(message.sha, diffLayerFrom(message.files));
        return;
      }
      if (message.kind !== 'file_content') return;
      // The answer to THIS canvas's ask: another seat's belongs to whoever
      // asked it, and the seat is named back for exactly this routing.
      if (subjectKey({ session: message.session }) !== seat) return;
      // **Stored under the scope that ASKED**, which is the one its sha
      // names - never the scope on screen. A reader who steps to another
      // commit before the answer lands still has it when they come back, and
      // the key is what the next `ensureContent` for that scope looks for;
      // keyed to the scope on screen it would be dropped, and the ask it was
      // remembered under would keep that scope control-less for good.
      contents.set(`${message.sha ?? 'all'}|${message.path}`, fileContentFrom(message.content));
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

  /**
   * One file's whole content, per scope, as the read answered it - and the
   * gaps the reader opened, per file.
   *
   * The lines a diff's hunks leave out are exactly what a between-hunk
   * control fills, and the record carries hunks alone; so this is asked for
   * per file, keyed by the scope it belongs to (a commit's content is not
   * the branch's).
   */
  const contents = new SvelteMap<string, FileContentRecord>();
  /** The reads already asked for, so a redraw does not ask twice. */
  const askedFrom = new SvelteSet<string>();
  const opened = new SvelteMap<string, SvelteMap<number, GapReveal>>();

  const contentKeyOf = (path: string): string => `${scopeCommit ?? 'all'}|${path}`;

  /** A file's carried lines, or `null` when its read refused or has not
   *  landed - the two cases no control is drawn for. */
  function contentFor(path: string): FileLines | null {
    const held = contents.get(contentKeyOf(path));
    if (held === undefined || held.outcome !== 'read') return null;
    return { lines: held.lines, truncated: held.truncated };
  }

  /** What the reader has opened on a file, by the hunk each gap follows. */
  function openedFor(path: string): SvelteMap<number, GapReveal> {
    const key = contentKeyOf(path);
    let held = opened.get(key);
    if (held === undefined) {
      held = new SvelteMap<number, GapReveal>();
      opened.set(key, held);
    }
    return held;
  }

  /**
   * Ask for one file's whole content, once per scope.
   *
   * **The read decides which controls exist at all**, which is why it is
   * asked when a section is drawn rather than when a control is pressed: a
   * deleted file, a subprocess that could not run, a read the ceiling cut -
   * each answers for itself, and a door that opened on one would be a press
   * that does nothing.
   */
  function ensureContent(path: string): void {
    const held = built.find((file) => file.path === path);
    if (held === undefined) return;
    // A file that can never yield a control is not asked about: no new side
    // to read (a deleted file, a binary, a submodule), no gaps to fill - an
    // added file's one hunk is its whole content, so nothing is left out -
    // no hunks at all, or hunks the record itself cut. `gapsOf` refuses the
    // last two for the same reason, and every one of them would cost a
    // subprocess for an answer no control can use.
    if (held.file.binary || held.file.submodule || held.file.status === 'deleted') return;
    if (held.file.status === 'added') return;
    if (held.file.truncated || held.file.hunks.length === 0) return;
    const key = contentKeyOf(path);
    if (contents.has(key) || askedFrom.has(key)) return;
    const open = connection;
    if (open === null || connState !== 'open') return;
    askedFrom.add(key);
    // An ask that did not go is not remembered: the next attach asks again.
    if (!open.fileContent(slot, path, scopeCommit)) askedFrom.delete(key);
  }

  $effect(() => {
    // Every file the canvas has drawn, and again whenever the connection
    // moves: a read asked while the socket was down never went.
    void connState;
    for (const path of sections.keys()) ensureContent(path);
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
    scheduleMark();
  }

  /**
   * The file the reader is inside, as the tree marks it: the section whose
   * region holds the canvas's own top edge, following the scroll.
   */
  let current = $state<string | null>(null);
  /** The drawn sections by path, tracked so the mark can measure them. */
  const sections = new SvelteMap<string, HTMLElement>();
  let canvasEl = $state<HTMLElement | null>(null);

  function markCurrent(): void {
    const canvas = canvasEl;
    if (canvas === null) return;
    // The edge is the canvas's own top at the wide width, where the list
    // scrolls inside it, and the window's at the narrow one, where the page
    // is the scroller.
    const edge = narrow ? 0 : canvas.getBoundingClientRect().top;
    const boxes = [...sections].map(([path, el]) => {
      const rect = el.getBoundingClientRect();
      return { path, top: rect.top, bottom: rect.bottom };
    });
    const at = currentFileAt(boxes, edge);
    if (at !== current) current = at;
  }

  /** One mark a frame: a scroll asks once per burst rather than per event. */
  let markFrame = 0;
  function scheduleMark(): void {
    if (markFrame !== 0) return;
    markFrame = requestAnimationFrame(() => {
      markFrame = 0;
      markCurrent();
    });
  }

  $effect(() => {
    const onScroll = (): void => scheduleMark();
    // Capture phase: a scroll of the list does not bubble, and this is the
    // one listener that hears both scrollers.
    window.addEventListener('scroll', onScroll, true);
    return () => {
      window.removeEventListener('scroll', onScroll, true);
      if (markFrame !== 0) cancelAnimationFrame(markFrame);
    };
  });

  /**
   * The tree's keyboard, the files page's own walk (Ved, 2026-10-17): the
   * arrows move the mark WITHOUT a prior focus, Home and End go to the ends,
   * moving onto a file scrolls to it and Enter folds a directory. Only the
   * vertical pair: on this page the horizontal arrows are the scope and
   * comment stepper's, so folding stays a press here.
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
      if (row.kind === 'file') jumpTo(row.path);
    };
    const step = (delta: number): void => {
      const paths = tree.map((row) => row.path);
      if (paths.length === 0) return;
      const from = keyRow === null ? paths.indexOf(current ?? '') : paths.indexOf(keyRow);
      const at = Math.min(Math.max((from === -1 ? -1 : from) + delta, 0), paths.length - 1);
      land(tree[at]);
    };
    const onkey = (event: KeyboardEvent): void => {
      if (keysOff) return;
      if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
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
        case 'Enter': {
          const row = tree.find((held) => held.path === keyRow);
          if (row === undefined) return;
          event.preventDefault();
          if (row.kind === 'dir') toggle(closedDirs, row.path);
          else jumpTo(row.path);
          break;
        }
        default:
          break;
      }
    };
    addEventListener('keydown', onkey);
    return () => removeEventListener('keydown', onkey);
  });

  // A row the Tab key lands on becomes the keyboard's row too, so the mark
  // follows real focus.
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

  // A change of scope, mode or set of drawn files moves the sections
  // without a scroll of its own.
  $effect(() => {
    void scopeCommit;
    void mode;
    void built;
    void narrow;
    scheduleMark();
  });

  /**
   * The step the canvas has already travelled to, by its own number.
   *
   * **It is what keeps the recovery from becoming a yank.** The jump below
   * tracks the built files, because a step pressed before its scope's layer
   * has landed - a socket scan, the async model build on a big branch - has
   * nothing to land on yet, and the models arriving is what re-runs the
   * jump. Once it HAS landed, a later rebuild (an opened gap, a scope or
   * mode switch) must leave the reader where they scrolled to, so the
   * landed number is the gate: not reactive, because writing it is not news
   * to anything.
   */
  let landedSeq = -1;

  /**
   * Bring one comment's card into view: its file first - the list may not
   * have drawn it at all - and then the anchored row, once that section is
   * up. The row is looked for over a few frames, because a virtualised
   * section the jump has just scrolled to is rendered a frame later.
   */
  $effect(() => {
    const ask = focusComment;
    if (ask === null) return;
    void ask.seq;
    void built;
    if (landedSeq === ask.seq) return;
    // Everything else is untracked: the jump reads the list, the canvases
    // and the folded set, and tracking those would re-run it on work that is
    // not this step's.
    untrack(() => {
      // A folded file is unfolded for the step: the card cannot come into
      // view while the section that draws it is closed.
      closedFiles.delete(ask.path);
      jumpTo(ask.path);
      let tries = 0;
      const look = (): void => {
        const el = canvases[ask.path];
        // A section that went between the frame and this look - the list
        // rebuilding under a step - has no row to find.
        const at = el === undefined || el === null ? null : anchorRow(el, ask.side, ask.line, mode);
        if (at !== null) {
          landedSeq = ask.seq;
          at.scrollIntoView({ block: 'center' });
          return;
        }
        if (tries < 8) {
          tries += 1;
          requestAnimationFrame(look);
        }
      };
      requestAnimationFrame(look);
    });
  });

  /** One drawn section, tracked for the reading mark. */
  function trackSection(node: HTMLElement, path: string): () => void {
    sections.set(path, node);
    return () => {
      sections.delete(path);
      scheduleMark();
    };
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
    const file = built.find((held) => held.path === path)?.file;
    if (file === undefined) return;
    for (const tr of el.querySelectorAll('tr.diff-line')) {
      if (mode === 'split') {
        for (const side of ['new', 'old'] as const) {
          const spot = doorSpot(tr, side, mode);
          // **A line the hunks do not carry takes no door.** An expanded
          // gap's lines are drawn here but the record does not hold them,
          // and a thread's anchor is resolved against what the server
          // ships: a comment written on one would park outdated the moment
          // it was filed.
          if (spot !== null && findLine(file, side, spot.line) !== null) {
            placeDoor(spot.cell, path, side, spot.line);
          }
        }
        continue;
      }
      const hit = rowSide(tr, null, mode);
      const cell = tr.querySelector('td');
      if (hit === null || !(cell instanceof HTMLElement)) continue;
      if (findLine(file, hit.side, hit.line) === null) continue;
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
      openBox({ path, side, line });
    });
    cell.append(door);
  }

  /** The comment box's own field, as its row mounts and goes. */
  let commentField = $state<HTMLElement | null>(null);
  /**
   * Whether the box being opened owes the reader the caret.
   *
   * **A new comment takes the caret** (Ved, 2026-10-16): the reader clicked
   * a line to write on, so the box it opened is where the words go - and the
   * row mounts a frame later, which is why the ask is held here rather than
   * spent at the click.
   */
  let focusPending = false;

  /** Open the comment box on a line, which is where a take's words will land. */
  function openBox(spot: ReviewSpot): void {
    // The box is already open on this very line - the reader clicked it
    // again - so it is the box the words are in: the click puts the caret
    // back in it and changes nothing else. A row is not rebuilt, so the ask
    // a fresh open leaves would never be spent, and clearing the draft here
    // would take words the reader had already typed.
    const same =
      box.spot !== null &&
      box.spot.path === spot.path &&
      box.spot.side === spot.side &&
      box.spot.line === spot.line;
    if (same) {
      commentField?.focus();
      return;
    }
    box.spot = spot;
    box.draft = '';
    box.line = null;
    focusPending = true;
  }

  /**
   * Close it, whatever the write's outcome was.
   *
   * What the box was DRAWING goes with it: the settings panel it was holding
   * open - nothing else on the page can close a panel with no box under it -
   * and the line the take under it left, which belongs to the attempt rather
   * than to the next line the reader opens.
   */
  function closeBox(): void {
    box.spot = null;
    box.draft = '';
    box.line = null;
    dictate.panel = false;
    focusPending = false;
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
   *
   * The comment card is a COMPONENT mounted by hand, because the row exists
   * only in the viewer's own table: the page holds the box's state and the
   * card reads it through the box, so a rebuilt row draws the same words.
   */
  $effect(() => {
    void scopeCommit;
    void mode;
    void closedFiles;
    void scoped;
    void box.spot;
    void reply.thread;
    void busy;
    void built;
    // A read landing, or a gap the reader opened, changes which controls are
    // wanted - and these reads run in the pass below, off a frame.
    for (const path of Object.keys(canvases)) {
      void contents.get(contentKeyOf(path));
      void opened.get(contentKeyOf(path));
    }
    const observers: MutationObserver[] = [];
    const draw = (el: HTMLDivElement, path: string) => {
      // Rows come and go under Svelte's own section swap as well as under this
      // pass, so every pass starts by letting go of the ones already gone.
      mounts.sweep();
      injectDoors(el, path);
      armClicks(el, path);
      const want: {
        key: string;
        side: 'old' | 'new';
        line: number;
        /** The control hangs above its anchor line rather than under it. */
        before?: boolean;
        build: (at: HTMLTableRowElement) => HTMLTableRowElement;
      }[] = [];
      if (!closedFiles.has(path)) {
        for (const thread of scoped) {
          if (thread.anchor.path !== path) continue;
          const side = thread.anchor.side === 'old' ? 'old' : 'new';
          const placed = canvasLine(thread, scopeCommit);
          if (placed === null) continue;
          const key = `t:${thread.id}:${statusName(thread.status)}:${thread.comments.length}:${
            reply.thread === thread.id ? 'open' : ''
          }`;
          want.push({
            key,
            side,
            line: placed.line,
            build: (at: HTMLTableRowElement) => threadRow(at, thread, placed, side),
          });
        }
        if (box.spot !== null && box.spot.path === path) {
          const spot = box.spot;
          want.push({
            key: `c:${spot.side}:${spot.line}`,
            side: spot.side,
            line: spot.line,
            build: (at: HTMLTableRowElement) => composerRow(at, spot),
          });
        }
        // The gap controls ride the hunk headers' own gutter cells, and the
        // below-the-last-hunk one rides a row of its own: one strip per gap
        // the read in hand can fill and the reader has not opened.
        const file = built.find((held) => held.path === path)?.file;
        if (file !== undefined) {
          decorateGaps(el, path, file, opened.get(contentKeyOf(path)), contentFor(path));
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
          // reshuffle each other every pass. A row that hangs ABOVE its
          // anchor has one place, and sits there or nowhere.
          if (entry.before === true) {
            if (at.previousElementSibling !== have) at.before(have);
          } else {
            let prev = have.previousElementSibling;
            while (prev !== null && prev.classList.contains('rv-insert')) {
              prev = prev.previousElementSibling;
            }
            if (prev !== at) at.after(have);
          }
          kept.add(entry.key);
          continue;
        }
        const row = entry.build(at);
        row.dataset['for'] = entry.key;
        if (entry.before === true) at.before(row);
        else at.after(row);
        kept.add(entry.key);
      }
      for (const [key, row] of existing) {
        if (kept.has(key)) continue;
        mounts.release(row);
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

  // The canvas going away - the other tab, the page - takes its rows' roots
  // with it: an effect of its own, because the pass above cleans up between
  // every pair of runs and must leave the rows it is keeping alone.
  $effect(() => () => mounts.releaseAll());

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
      // The rule the doors keep: a line the hunks do not carry - an expanded
      // gap's own line - opens nothing.
      const file = built.find((held) => held.path === path)?.file;
      if (file === undefined || findLine(file, hit.side, hit.line) === null) return;
      openBox({ path, side: hit.side, line: hit.line });
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

  /** The roots the rows built here own, released where a row is dropped and
   *  swept where one leaves without the reconciler seeing it. */
  const mounts = new Mounts();

  /** One turn, drawn by the piece both halves of the page draw. */
  function turnBlock(turn: ReviewThreadRecord['comments'][number], index: number): HTMLElement {
    const host = document.createElement('div');
    const held = mount(ThreadTurn, {
      target: host,
      // The clock stays live through the getter: a row the reconciler keeps
      // is never rebuilt, so a value here would freeze its age chip.
      props: {
        turn,
        rounds: reviews,
        index,
        get now() {
          return now;
        },
      },
    });
    mounts.hold(host, () => void unmount(held));
    return host;
  }

  function threadCard(
    thread: ReviewThreadRecord,
    placed: { line: number; movedFrom: number | null },
  ): HTMLElement {
    const card = document.createElement('article');
    card.className = 'rv-thread compact';
    const tops = document.createElement('div');
    tops.className = 'rv-tops';
    tops.append(mark(statusName(thread.status)));
    tops.append(span(`rv-st ${statusName(thread.status)}`, statusName(thread.status)));
    tops.append(
      span('rv-where', `${thread.anchor.path.split('/').pop() ?? ''} · line ${placed.line}`),
    );
    if (placed.movedFrom !== null) {
      tops.append(span('rv-moved', `moved from ${placed.movedFrom}`));
    }
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
      reply.thread = reply.thread === thread.id ? null : thread.id;
      reply.draft = '';
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
    if (reply.thread === thread.id) {
      // **The same words box the comment is, mounted the same way.** A reply
      // takes the mic, the take's bar and the settings panel exactly as the
      // comment box does, and one component draws both halves' boxes - the
      // conversation half mounts this one too.
      card.append(replyCard(thread));
    }
    return card;
  }

  /** The reply box under one thread, over the page's reply state. */
  function replyCard(thread: ReviewThreadRecord): HTMLElement {
    const host = document.createElement('div');
    const card = mount(WordsBox, {
      target: host,
      props: {
        class: 'rv-reply',
        rows: 2,
        placeholder: 'Respond to this thread...',
        label: 'your response',
        sendLabel: 'send',
        box: reply,
        voice,
        onsend: (text: string) => {
          // The box closes only once the write landed: a refusal keeps the
          // reader's words where they wrote them.
          void onreply(thread, text).then((filed) => {
            if (!filed) return;
            reply.thread = null;
            reply.draft = '';
          });
        },
        oncancel: () => {
          reply.thread = null;
          reply.draft = '';
        },
        onpress: () => onpress('reply'),
        onhold: () => onhold('reply'),
        field: (el: HTMLElement | null) => onelement('reply', el),
      },
    });
    mounts.hold(host, () => void unmount(card));
    return host;
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
      wrap.append(threadCard(thread, placed));
    }
    return insertRow(at, side, mode, wrap);
  }

  /** The hunk header row the line at `at` sits under. */
  function headerRow(at: HTMLTableRowElement): HTMLTableRowElement | null {
    let row: Element | null = at.previousElementSibling;
    while (row !== null) {
      if (
        row instanceof HTMLTableRowElement &&
        row.querySelector('td[class*="hunk-action"]') !== null
      ) {
        return row;
      }
      row = row.previousElementSibling;
    }
    return null;
  }

  /**
   * One gap's strip, drawn where GitHub draws it (Ved, 2026-10-16, from the
   * look): **a gutter-cell control with GitHub's own glyph and nothing
   * else** - no count, no word of this page's own on screen, and GitHub's
   * own wording as the tooltip. The accessible name still says what the
   * press would do and to which file, which the glyph alone cannot.
   */
  function gapButton(path: string, gap: Gap, way: GapWay, remaining: number): HTMLButtonElement {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'rv-gapbtn';
    button.title = way === 'all' ? 'Expand All' : way === 'down' ? 'Expand Down' : 'Expand Up';
    const step = Math.min(GAP_STEP, remaining);
    const opens = way === 'all' ? remaining : step;
    const which = way === 'all' ? 'all' : way === 'down' ? 'the next' : 'the previous';
    button.setAttribute(
      'aria-label',
      `show ${which} ${opens} hidden ${opens === 1 ? 'line' : 'lines'} of ${path}`,
    );
    const glyph = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    glyph.setAttribute('class', 'ic');
    const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
    use.setAttribute(
      'href',
      way === 'all' ? '#i-unfold' : way === 'down' ? '#i-fold-down' : '#i-fold-up',
    );
    glyph.append(use);
    button.append(glyph);
    button.addEventListener('click', () => void expand(path, gap.after, way));
    return button;
  }

  /** A row shaped after a hunk header row: every cell it has, empty. */
  function gapRow(shape: HTMLTableRowElement): HTMLTableRowElement {
    const row = document.createElement('tr');
    row.className = 'rv-gaprow';
    for (const cell of shape.children) {
      const copy = cell.cloneNode(false);
      if (copy instanceof HTMLTableCellElement) {
        copy.textContent = '';
        row.append(copy);
      }
    }
    return row;
  }

  /**
   * The gap controls of one drawn file, checked into the hunk headers - and
   * the one gap below the last hunk into a row of its own.
   *
   * **GitHub's own placement, taken from its page** (Ved, 2026-10-16, from
   * the look): the gap above the first hunk and the gap between two hunks
   * ride the following hunk's header, in its gutter cell; the gap below the
   * last hunk has no header after it, so it gets a row after the last line,
   * empty but for the strip. A gap with no more than [`GAP_SMALL`] lines
   * left opens in one press and takes the two-way glyph; a longer one takes
   * a strip per end it can be opened from - a pair between hunks, one
   * direction at the file's edges.
   *
   * **A reconciling pass, like the row one**: a strip already in its own
   * place is left alone, one whose gap is closed or whose row the viewer has
   * not drawn is removed or skipped, so the pass reaches a quiet steady
   * state instead of re-arming the mutations that call it.
   */
  function decorateGaps(
    el: HTMLDivElement,
    path: string,
    file: DiffFileRecord,
    reveals: SvelteMap<number, GapReveal> | undefined,
    content: FileLines | null,
  ): void {
    const last = file.hunks.length - 1;
    const wanted = new SvelteMap<string, { gap: Gap; way: GapWay; hunk: number | null }>();
    for (const gap of gapsOf(file, content)) {
      const remaining = gapRemaining(gap, reveals?.get(gap.after));
      if (remaining === 0) continue;
      const ways: GapWay[] =
        remaining <= GAP_SMALL
          ? ['all']
          : gap.after === -1
            ? ['up']
            : gap.after === last
              ? ['down']
              : ['down', 'up'];
      for (const way of ways) {
        wanted.set(`g:${gap.after}:${way}`, {
          gap,
          way,
          hunk: gap.after === last ? null : gap.after + 1,
        });
      }
    }
    for (const button of [...el.querySelectorAll('button.rv-gapbtn')]) {
      const key = button instanceof HTMLElement ? button.dataset['for'] : undefined;
      if (key === undefined || !wanted.has(key)) button.remove();
    }
    // The gap below the last hunk lives in a row of its own, shaped after the
    // hunk header rows so its columns line up with theirs.
    const rows = new SvelteSet<string>();
    for (const [key, want] of wanted) {
      if (want.hunk !== null) continue;
      const rowKey = `r:${want.gap.after}`;
      rows.add(rowKey);
      let row = el.querySelector<HTMLTableRowElement>(`tr.rv-gaprow[data-for="${rowKey}"]`);
      if (row === null) {
        const lines = file.hunks[last]?.lines ?? [];
        const tail = lines[lines.length - 1];
        const side = tail?.kind === 'removed' ? 'old' : 'new';
        const line = tail === undefined ? null : side === 'old' ? tail.oldLine : tail.newLine;
        const at = line === null ? null : anchorRow(el, side, line, mode);
        const shape = at === null ? null : headerRow(at);
        if (at !== null && shape !== null) {
          row = gapRow(shape);
          row.dataset['for'] = rowKey;
          at.after(row);
        }
      }
      if (row === null) continue;
      const cell = row.querySelector('td[class*="hunk-action"]');
      if (!(cell instanceof HTMLElement)) continue;
      cell.classList.add('rv-hunkcell');
      if (row.querySelector(`button.rv-gapbtn[data-for="${key}"]`) === null) {
        const button = gapButton(
          path,
          want.gap,
          want.way,
          gapRemaining(want.gap, reveals?.get(want.gap.after)),
        );
        button.dataset['for'] = key;
        cell.append(button);
      }
    }
    for (const row of [...el.querySelectorAll('tr.rv-gaprow')]) {
      const held = row instanceof HTMLElement ? row.dataset['for'] : undefined;
      if (held === undefined || !rows.has(held)) row.remove();
    }
    for (const [key, want] of wanted) {
      if (want.hunk === null) continue;
      if (el.querySelector(`button.rv-gapbtn[data-for="${key}"]`) !== null) continue;
      const first = file.hunks[want.hunk]?.lines[0];
      if (first === undefined) continue;
      const side = first.kind === 'removed' ? 'old' : 'new';
      const line = side === 'old' ? first.oldLine : first.newLine;
      if (line === null) continue;
      const at = anchorRow(el, side, line, mode);
      const header = at === null ? null : headerRow(at);
      const cell = header?.querySelector('td[class*="hunk-action"]');
      if (!(cell instanceof HTMLElement)) continue;
      cell.classList.add('rv-hunkcell');
      const button = gapButton(
        path,
        want.gap,
        want.way,
        gapRemaining(want.gap, reveals?.get(want.gap.after)),
      );
      button.dataset['for'] = key;
      cell.append(button);
    }
  }

  /**
   * Open one gap, one press at a time: the file's own diff text is
   * re-synthesized with what the reader has now revealed, and that one table
   * re-renders.
   *
   * **A press moves one end, by [`GAP_STEP`] lines** (GitHub's own step,
   * taken from its page): the down strip opens the lines the previous hunk
   * left off at, the up strip the lines just above the next hunk, and the
   * one-way strip a short gap has opens the rest in a single press. The text
   * is re-emitted rather than rows spliced in, because the viewer parses
   * diff text and computes each row's line numbers from the heading - rows
   * inserted by hand would draw a numbering the viewer never agreed to.
   */
  async function expand(path: string, after: number, way: GapWay): Promise<void> {
    const held = built.find((file) => file.path === path);
    const content = contentFor(path);
    if (held === undefined || content === null) return;
    const gap = gapsOf(held.file, content).find((g) => g.after === after);
    if (gap === undefined) return;
    const key = scopeCommit ?? 'all';
    const openedHere = openedFor(path);
    const reveal = openedHere.get(after) ?? { down: 0, up: 0 };
    const remaining = gapRemaining(gap, reveal);
    if (remaining === 0) return;
    const step = Math.min(GAP_STEP, remaining);
    // "all" takes the whole gap; which end it lands in only decides which
    // hunk carries the lines, and the top gap has no hunk before it.
    openedHere.set(
      after,
      way === 'down'
        ? { down: reveal.down + step, up: reveal.up }
        : way === 'up'
          ? { down: reveal.down, up: reveal.up + step }
          : { down: after === -1 ? 0 : gap.count, up: after === -1 ? gap.count : 0 },
    );
    const { DiffFile } = await import('@git-diff-view/core');
    // A build that lands after the scope moved on is the old scope's.
    if ((scopeCommit ?? 'all') !== key) return;
    const instance = new DiffFile(
      held.file.oldPath ?? held.file.path,
      '',
      held.file.path,
      '',
      [synthesizeFileDiff(held.file, { content, expanded: openedHere })],
      '',
      '',
    );
    (instance as { init?: () => void }).init?.();
    built = built.map((file) =>
      file.path === path ? { path: file.path, diffFile: instance, file: file.file } : file,
    );
  }

  function commentSpot(): ReviewSpot | null {
    return box.spot;
  }

  /**
   * File the words the box is holding on the line it was opened on.
   *
   * The anchor is captured where the lines are: the row knows only the spot,
   * so the line's text and its neighbours within the hunk come from the scope
   * this page holds - the capture the crate's resolver defines.
   */
  function fileComment(text: string): void {
    const at = commentSpot();
    if (at === null) return;
    const file = scopeFiles.find((held) => held.path === at.path);
    const found = file === undefined ? null : findLine(file, at.side, at.line);
    if (found === null) return;
    const line = found.hunk.lines[found.index];
    if (line === undefined) return;
    // As with a reply: the composer stays open, holding the words, until the
    // write lands.
    void oncomment({
      path: at.path,
      side: at.side,
      line: at.line,
      text,
      context: captureContext(found.hunk.lines, found.index),
      lineText: line.text,
    }).then((filed) => {
      if (!filed) return;
      closeBox();
    });
  }

  /**
   * The comment box: a component mounted into the row, over the box state the
   * page holds.
   *
   * Its card is drawn by `WordsBox` - the words, the acts and the mic - because
   * a row built from `createElement` cannot wear the shared dictation control,
   * and re-deriving one here is what the review's box and the composer's box
   * are meant to share. **Every live value goes in as a getter**: a mount
   * reads its props once, and a take starting under the reader has to redraw
   * the bar without the row being rebuilt.
   */
  function composerRow(at: HTMLTableRowElement, spot: ReviewSpot): HTMLTableRowElement {
    const wrap = document.createElement('div');
    wrap.className = 'rv-inline';
    const card = mount(WordsBox, {
      target: wrap,
      props: {
        class: 'rv-thread compact comment',
        headline: `${spot.path} · line ${spot.line}`,
        rows: 3,
        placeholder: 'Comment on this line...',
        label: 'your comment',
        sendLabel: 'comment',
        box,
        voice,
        onsend: fileComment,
        oncancel: closeBox,
        onpress: () => onpress('comment'),
        onhold: () => onhold('comment'),
        field: (el: HTMLElement | null) => {
          commentField = el;
          onelement('comment', el);
          // The row the click opened has just mounted: the caret the reader
          // owes goes in with it.
          if (el !== null && focusPending) {
            focusPending = false;
            el.focus();
          }
        },
      },
    });
    mounts.hold(wrap, () => void unmount(card));
    return insertRow(at, spot.side, mode, wrap);
  }
</script>

<FileTree
  rows={tree}
  heading="Files"
  label="files in this scope"
  count={scopeFiles.length}
  closed={closedDirs}
  here={current}
  active={keyRow}
  renamed={(path) => (fileByPath.get(path)?.oldPath ?? null) !== null}
  ondir={(path) => toggle(closedDirs, path)}
  onfile={jumpTo}
>
  {#snippet trailing(row: TreeRow<unknown>)}
    {@const file = fileByPath.get(row.path)}
    {#if file !== undefined}
      {@const at = lineCounts(file)}
      {@const mine = scoped.filter(
        (thread) => thread.anchor.path === file.path && canvasLine(thread, scopeCommit) !== null,
      )}
      <span class="rv-fstat">
        {#if at.plus > 0}<span class="rv-plus">+{at.plus}</span>{/if}
        {#if at.minus > 0}<span class="rv-minus">-{at.minus}</span>{/if}
      </span>
      <span class="rv-fcount">
        {#if mine.length}<span class="rv-ccnum">{mine.length}</span>{/if}
      </span>
    {/if}
  {/snippet}
</FileTree>

<main class="rv-canvas" bind:this={canvasEl}>
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
    (thread) => thread.anchor.path === file.path && canvasLine(thread, scopeCommit) !== null,
  )}
  <!-- The floor is the section's, and the MEASUREMENT is its inner body's:
       an observer on the floored section records the clamp, so the height
       can never shrink and a collapsed file leaves a blank as tall as its
       diff (measured: a section's rect and min-height both 7887px after the
       collapse, its .rv-diff gone). -->
  <section
    class="rv-file"
    id={file.path}
    {@attach (node: HTMLElement) => trackSection(node, file.path)}
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

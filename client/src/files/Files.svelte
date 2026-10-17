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
   * **Every box dictates** (Ved, 2026-10-17: dictation in a text box is a
   * hard rule): the comment, the reply and the finish summary wear the mic
   * through the composer's own `WordsBox`, over this page's take - the same
   * arrangement the review page has, ported whole so the two pages' boxes
   * behave alike.
   *
   * Its states are drawn here: nothing yet (reading), a seat the server
   * refused, and a tree with no files.
   */
  import { SvelteSet } from 'svelte/reactivity';
  import { mount, unmount } from 'svelte';

  import FileTree from '../components/FileTree.svelte';
  import { toggleClosed, type TreeRow } from '../components/tree';
  import { boxKey } from '../composer/box.svelte';
  import {
    bindPushToTalk,
    Dictation,
    openCapture,
    takeCounts,
    takePhase,
    type RowPhase,
    type TakeLine,
  } from '../composer/dictate.svelte';
  import { axesFor, deviceFor, rememberAxes, rememberDevice } from '../composer/dictation';
  import { Landing } from '../composer/landing';
  import { joined, noticeLine } from '../composer/view';
  import { composerFrom, filesFrom, filesOnly } from '../composer/wire';
  import { hrefForSlot } from '../routes';
  import type { CommentBox, ReplyBox, Voice, Words } from '../review/boxes';
  import Conversation from '../review/Conversation.svelte';
  import { latestRound, roundHasWork, roundsNewestFirst } from '../review/view';
  import WordsBox from '../review/WordsBox.svelte';
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
  import { watchSession, type SessionRead } from '../session/live';
  import type { ReviewThreadRecord, SessionRecord } from '../session/wire';
  import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import Inline from './Inline.svelte';
  import Rail from './Rail.svelte';
  import Reader from './Reader.svelte';
  import { contextForComment, railRows, treeScope } from './comments';
  import type { ReaderBlock, ReaderHandle } from './reader';
  import { indexTreeRows, matchedEntries } from './tree';

  let {
    connection,
    slot,
    dictation,
  }: {
    connection: Connection;
    slot: SessionSlot;
    /** Whether this install can dictate at all, which gates every mic. */
    dictation: boolean;
  } = $props();

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
    record?.tree_reviews.reviews.outcome === 'read'
      ? roundsNewestFirst(record.tree_reviews.reviews.value)
      : [],
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

  /**
   * The three boxes, held on the page as the review page holds its own: a
   * take's words and its own line have to reach the box that began the take,
   * and the reader is re-mounted under its boxes.
   */
  type BoxName = 'comment' | 'reply' | 'summary';
  const comment = $state<CommentBox>({
    spot: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    panel: false,
    take: null,
    wire: null,
  });
  const reply = $state<ReplyBox>({
    thread: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    panel: false,
    take: null,
    wire: null,
  });
  const summary = $state<Words>({
    draft: '',
    busy: false,
    line: null,
    phase: null,
    panel: false,
    take: null,
    wire: null,
  });

  /** Where a take's words belong: the box whose mic began it, stamped at the
   *  press - and the spot/thread it held then, so a box that has MOVED is no
   *  longer the box the take belongs to. */
  let takeIn = $state<BoxName | null>(null);
  let takeSpot = $state<CommentBox['spot']>(null);
  let takeThread = $state<string | null>(null);
  /** Where the last mic was pressed: the box a refusal is owed to. */
  let pressedIn = $state<BoxName | null>(null);
  /** Where a page line about a take is drawn: the box the attempt was made in. */
  let lineIn = $state<BoxName | null>(null);
  /** Which box's mic opened the settings, which is the box that anchors the panel. */
  let panelIn = $state<BoxName | null>(null);
  let dictateLine = $state<TakeLine | null>(null);
  const landing = new Landing();
  const elements: Record<BoxName, HTMLElement | null> = {
    comment: null,
    reply: null,
    summary: null,
  };

  const defaults = $derived(connection.settings()?.dictate ?? DEFAULT_AXES);
  let axesEdit = $state<DictateAxes | null>(null);
  let deviceEdit = $state<string | null>(null);
  const key = $derived(boxKey(slot));
  const seatAxes = $derived(axesEdit ?? axesFor(key, defaults));
  const seatDevice = $derived(deviceEdit ?? deviceFor(key));

  function setAxes(next: DictateAxes): void {
    axesEdit = next;
    rememberAxes(key, next);
  }

  function setDevice(next: string | null): void {
    deviceEdit = next;
    rememberDevice(key, next);
  }

  const composer = $derived(composerFrom(record?.composer));

  const dictate = new Dictation({
    connection: () => connection,
    seat: () => slot,
    shown: () => composer.take !== null,
    options: () => axesFor(boxKey(slot), defaults),
    device: () => deviceFor(boxKey(slot)),
    capture: openCapture,
    onStart: () => {
      takeIn = pressedIn;
      lineIn = pressedIn;
      takeSpot = comment.spot;
      takeThread = reply.thread;
      dictateLine = null;
      landing.arm(boxKey(slot), composer.notice);
    },
    onLine: (line) => {
      lineIn = takeIn;
      dictateLine = line;
      landing.drop();
    },
    onRefused: (line) => {
      lineIn = pressedIn;
      dictateLine = line;
    },
  });

  const rowPhase = $derived(takePhase(dictate, composer.take, slot));
  const rowWire = $derived(takeCounts(dictate, slot));

  /**
   * The box a key-begun take belongs to: the one the reader's caret is in,
   * else the one drawing, else the summary - whose box is always in the
   * rail. The review page's own rule, so the hold behaves the same on both.
   *
   * **The rail's box counts only while the rail is up**: it is the third
   * column when the width allows, and a sheet that must be raised when it
   * does not - a take whose words landed in a box behind a closed sheet
   * would be a take the reader loses.
   */
  function pttBox(): BoxName | null {
    const active = document.activeElement;
    if (active !== null && active !== document.body) {
      for (const at of ['comment', 'reply', 'summary'] as const) {
        if (elements[at] === active) return at;
      }
    }
    // The reader's boxes count only while the reader is the half that is up.
    if (tab === 'files') {
      if (comment.spot !== null) return 'comment';
      if (reply.thread !== null) return 'reply';
    }
    // A take already running keeps its box, so the key's release can end it.
    if (takeIn !== null) return takeIn;
    // The summary is the rail's box, and the rail is the files half's.
    if (tab !== 'files') return null;
    if (record === null || entries.length === 0) return null;
    const railUp = sheet === 'comments' || !window.matchMedia('(max-width: 980px)').matches;
    return railUp ? 'summary' : null;
  }

  // The push-to-talk key, once for the page: a hold begins a take in the box
  // it belongs to, exactly as the composer's own box does.
  bindPushToTalk({
    dictate: () => dictate,
    offered: () => dictation && pttBox() !== null,
    bind: () => composer.bind,
    mode: () => composer.mode,
    onBegin: () => {
      const at = pttBox();
      if (at === null) return;
      pressedIn = at;
      elements[at]?.focus();
    },
  });

  /** One stable object whose fields are getters: a mounted box reads its
   *  props once, and the panel must never show a frozen axes snapshot. */
  const voice: Voice = {
    dictate,
    get offered() {
      return dictation;
    },
    get connection() {
      return connection;
    },
    get bind() {
      return composer.bind;
    },
    get mode() {
      return composer.mode;
    },
    get axes() {
      return seatAxes;
    },
    get device() {
      return seatDevice;
    },
    get defaults() {
      return defaults;
    },
    onaxes: setAxes,
    ondevice: setDevice,
  };

  /** One box's own take row, or `null` when the take is another box's. */
  function phaseIn(at: BoxName): RowPhase | null {
    return takeIn === at ? rowPhase : null;
  }

  /** What each box reads off the page's state: the write in flight, the
   *  record's take and this page's counts, and the two per-box readings. */
  $effect(() => {
    for (const at of ['comment', 'reply', 'summary'] as const) {
      const held = at === 'comment' ? comment : at === 'reply' ? reply : summary;
      held.busy = busy;
      held.take = composer.take;
      held.wire = rowWire;
      held.phase = phaseIn(at);
      held.panel = dictate.panel && panelIn === at;
    }
  });

  /** A take goes with its box: the box that began a LIVE take going - the
   *  reader's cancel, a move to another spot or thread - ends the take. Only
   *  a live take; one already submitted carries its words to the landing.
   *  The summary's box is always on the page, so its takes never go with it. */
  $effect(() => {
    if (takeIn === null) return;
    const gone =
      takeIn === 'summary'
        ? false
        : takeIn === 'comment'
          ? comment.spot === null ||
            takeSpot === null ||
            comment.spot.path !== takeSpot.path ||
            comment.spot.line !== takeSpot.line
          : reply.thread === null || reply.thread !== takeThread;
    const live = dictate.take;
    if (gone && (dictate.opening || (live !== null && !live.submitted))) {
      landing.drop();
      dictate.act('cancel');
    }
  });

  /** A landed take puts its words in the box that began it, once. */
  $effect(() => {
    const words = landing.place(boxKey(slot), composer.notice);
    if (words === null) return;
    if (
      composer.take === null &&
      composer.notice?.kind === 'landed' &&
      composer.notice.carried === true
    ) {
      return;
    }
    landing.placed(composer.notice);
    if (takeIn === null) return;
    if (takeIn === 'comment') {
      if (
        comment.spot === null ||
        takeSpot === null ||
        comment.spot.path !== takeSpot.path ||
        comment.spot.line !== takeSpot.line
      ) {
        comment.spot = takeSpot;
      }
      if (comment.spot === null) return;
      comment.draft = joined(comment.draft, words);
    } else if (takeIn === 'reply') {
      if (reply.thread === null || reply.thread !== takeThread) reply.thread = takeThread;
      if (reply.thread === null) return;
      reply.draft = joined(reply.draft, words);
    } else {
      summary.draft = joined(summary.draft, words);
    }
    elements[takeIn]?.focus();
  });

  /** The line a take owes, in the box it belongs to. */
  $effect(() => {
    const line = dictateLine ?? noticeLine(composer.notice, landing.holds(composer.notice));
    const at = dictateLine !== null ? lineIn : (takeIn ?? 'comment');
    comment.line = at === 'comment' ? line : null;
    reply.line = at === 'reply' ? line : null;
    summary.line = at === 'summary' ? line : null;
  });

  /**
   * What `finish review` seals: every thread carrying an unfiled USER turn,
   * read off the record rather than off a set of ids this sitting happened
   * to write. The page-local set said "(1) ready" while the button stayed
   * disabled after a reload took the memory with it (measured live, Ved
   * 2026-10-17); the record always knows what is unsealed.
   */
  const unsealed = $derived(
    treeThreads.filter((thread) =>
      thread.comments.some((turn) => turn.author.kind === 'user' && turn.review_id === null),
    ),
  );
  let busy = $state(false);
  let refusal = $state<string | null>(null);
  let now = $state(Date.now());

  /**
   * Which half of the page is up: the tree and the reader, or the
   * conversation - the seat-tree scope's rounds, newest first (Ved,
   * 2026-10-17: "just like how the git diff"). Defaults to the files, which
   * is what the page is for; the rail stays drawn on both.
   */
  let tab = $state<'files' | 'conversation'>('files');

  /** The threads carrying a turn nobody filed, for the conversation's rows. */
  const draftIds = $derived(new Set(unsealed.map((thread) => thread.id)));

  /** Whether both tree_reviews halves landed - what the empty copy waits for. */
  const treeRead = $derived(
    record !== null &&
      record.tree_reviews.threads.outcome === 'read' &&
      record.tree_reviews.reviews.outcome === 'read',
  );

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
      if (tab !== 'files') return;
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

  /** A line was pressed in the reader: the composer opens on it. The spot's
   *  side is always the new one - a file has no diff sides. */
  function commentOn(line: number): void {
    if (open === null) return;
    comment.spot = { path: open, side: 'new', line };
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
    closeComposer();
  }

  async function replied(thread: ReviewThreadRecord, text: string): Promise<boolean> {
    reply.busy = true;
    const { went } = await write(upsertCommand(slot, treeScope(slot), withReply(thread, text)));
    reply.busy = false;
    if (!went) return false;
    reply.thread = null;
    reply.draft = '';
    return true;
  }

  async function resolved(thread: ReviewThreadRecord): Promise<void> {
    await write(statusCommand(slot, treeScope(slot), thread.id, 'Resolved'));
  }

  async function reopened(thread: ReviewThreadRecord): Promise<void> {
    await reopenThread(sends, slot, treeScope(slot), thread.id, latestRound(treeRounds, thread));
  }

  async function finished(text: string | null): Promise<void> {
    const ids = unsealed.map((thread) => thread.id);
    if (ids.length === 0) return;
    summary.busy = true;
    const sealed = await finishRound(
      sends,
      slot,
      treeScope(slot),
      ids,
      text,
      roundHasWork(treeThreads, ids),
    );
    summary.busy = false;
    if (!sealed) return;
    summary.draft = '';
  }

  /** A rail row: open its file and put the reader on its line - from the
   *  conversation too, where the reader is not the tab that is up. */
  function jump(row: (typeof rows)[number]): void {
    tab = 'files';
    open = row.thread.anchor.path;
    pendingLine = row.line ?? row.thread.anchor.line;
  }

  const shown = $derived(matchedEntries(entries, query));
  const tree = $derived(indexTreeRows(shown, closed));

  /** Comments dropped on one file, from the seat-tree half - the row's pill. */
  function droppedCount(path: string): number {
    return treeThreads.filter((thread) => thread.anchor.path === path).length;
  }

  /** A status as a block-key segment: the named states as themselves, an
   *  unnamed one as one word rather than a stringified object. */
  function statusKey(status: ReviewThreadRecord['status']): string {
    return typeof status === 'string' ? status : 'unnamed';
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
        // The key carries the thread's own state: a card the key keeps is a
        // card CM does not re-mount, and a resolve or a reply would leave
        // its stale DOM (acts, chip) under the line (measured live, Ved
        // 2026-10-17).
        key: `thread:${id}:${statusKey(row.thread.status)}:${row.thread.comments.length}:${reply.thread === id ? 'reply' : 'shut'}`,
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
              voice,
              onrespond: () => {
                reply.thread = reply.thread === id ? null : id;
                reply.draft = '';
              },
              onreply: (text: string) => void replied(row.thread, text),
              onresolve: () => void resolved(row.thread),
              onreopen: () => void reopened(row.thread),
              onpress: () => (pressedIn = 'reply'),
              onhold: () => (panelIn = 'reply'),
              field: (el: HTMLElement | null) => {
                elements.reply = el;
                // The caret goes in with the box (Ved, 2026-10-17): respond
                // opens the box AND the keyboard, so no press is needed
                // before typing.
                el?.focus();
              },
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
          const held = mount(WordsBox, {
            target: host,
            props: {
              class: 'rv-thread comment',
              headline: `${spot.path.split('/').pop() ?? ''} · line ${spot.line}`,
              placeholder: 'Comment on this line...',
              label: 'your comment',
              sendLabel: 'comment',
              box: comment,
              voice,
              onsend: (text: string) => void commented(text),
              oncancel: closeComposer,
              onpress: () => (pressedIn = 'comment'),
              onhold: () => (panelIn = 'comment'),
              field: (el: HTMLElement | null) => {
                elements.comment = el;
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
      {#if tab === 'files'}
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
      {/if}
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
    <nav class="rv-tabs" aria-label="files sections">
      <button
        type="button"
        class:on={tab === 'conversation'}
        onclick={() => {
          tab = 'conversation';
          // The sheets are the files half's doors; one left open would hang
          // over the conversation with nothing behind it.
          closeSheet();
        }}
      >
        Conversation
      </button>
      <button type="button" class:on={tab === 'files'} onclick={() => (tab = 'files')}>
        Files <span class="rv-tn">{entries.length}</span>
      </button>
    </nav>
    <div class="files-body" class:conv={tab === 'conversation'}>
      {#if tab === 'files'}
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
      {:else}
        <main class="files-main files-conv" aria-label="the conversation">
          <Conversation
            threads={treeThreads}
            rounds={treeRounds}
            drafts={draftIds}
            {busy}
            {now}
            {voice}
            {reply}
            noun="tree"
            known={treeRead}
            wide
            onreply={replied}
            onresolve={(thread: ReviewThreadRecord) => void resolved(thread)}
            onreopen={(thread: ReviewThreadRecord) => void reopened(thread)}
            onpress={() => (pressedIn = 'reply')}
            onhold={() => (panelIn = 'reply')}
            onfield={(el: HTMLElement | null) => (elements.reply = el)}
          />
        </main>
      {/if}
      {#if tab === 'files'}
        <div class="files-railhold" class:sheet={sheet === 'comments'}>
          <button class="files-sheetclose" type="button" onclick={closeSheet}>done</button>
          <Rail
            {rows}
            {repliesToRead}
            finishCount={unsealed.length}
            {busy}
            summaryBox={summary}
            {voice}
            onjump={jump}
            onfinish={(text) => void finished(text)}
            onpress={() => (pressedIn = 'summary')}
            onhold={() => (panelIn = 'summary')}
            onfield={(el) => (elements.summary = el)}
          />
        </div>
      {/if}
    </div>
  {/if}
</div>

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
   * comment target, with a source-line gutter beside it**: a press on the
   * block comments on the whole section, a press on a line number comments
   * on that exact line with the line first in its captured context. A
   * plain-text document has no blocks, so its rows are its lines.
   *
   * **Threads draw where this version lets them.** The server re-anchors
   * each doc thread by its first captured line, the way a code thread is
   * re-anchored; the viewer draws a moved thread at its new line, one the
   * revision left behind at its captured line marked outdated, and only a
   * thread whose line is past the document collects at the end with the
   * reason.
   *
   * **The door is the hand-off's answer.** The drafts are filed as one
   * review through the same commands a code review rides, and the parked
   * call is released with **approve** or **something is off** - each with
   * the optional note - so the agent hears the reading's result and never
   * reads a bare done as approval.
   */
  import { SvelteSet } from 'svelte/reactivity';

  import { report, type Connection } from '../socket';
  import { mintPromptId } from '../wire/ids';
  import { goTo } from '../routes';
  import Prose from '../chat/Prose.svelte';
  import type { DocumentKind } from '../protocol';
  import { boxKey } from '../composer/box.svelte';
  import {
    bindPushToTalk,
    Dictation,
    openCapture,
    takeCounts,
    takePhase,
    takeSilenceLine,
    type TakeLine,
  } from '../composer/dictate.svelte';
  import { axesFor, deviceFor, rememberAxes, rememberDevice } from '../composer/dictation';
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import Field from '../composer/Field.svelte';
  import { Landing } from '../composer/landing';
  import TakeCard from '../composer/TakeCard.svelte';
  import { joined, noticeLine } from '../composer/view';
  import { composerFrom } from '../composer/wire';
  import type { ReplyBox, TurnEditor, Voice } from '../review/boxes';
  import ResolvedMarker from '../review/ResolvedMarker.svelte';
  import ThreadDelete from '../review/ThreadDelete.svelte';
  import ThreadTurn from '../review/ThreadTurn.svelte';
  import { statusName } from '../review/view';
  import {
    deleteCommand,
    statusCommand,
    submitCommand,
    upsertCommand,
    withEditedTurn,
  } from '../review/write';
  import WordsBox from '../review/WordsBox.svelte';
  import { watchSession, type SessionRead } from '../session/live';
  import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import {
    addDraft,
    clearDrafts,
    docScope,
    docThread,
    draftsFor,
    editDraftText,
    lineContext,
    linesOf,
    placeThreads,
    rowsOf,
    type DocDraft,
    type DocRow,
    type DocThreadPlacement,
  } from './comments';
  import { watchDocument, type DocRead } from './live';
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
    dictation = false,
  }: {
    slot: SessionSlot;
    path: string;
    reviewable: boolean;
    /** The parked hand-off's id, when this route came from an ask's Open;
     *  null on a deep link, where there is no call to release. */
    handoff?: string | null;
    connection: Connection;
    /** Whether this install can dictate at all, which gates the comment
     *  box's mic - the install's own answer, read off the home. */
    dictation?: boolean;
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

  /** The document's own lines, the numbering a comment anchors by. */
  const lines = $derived(linesOf(text));

  /**
   * Every thread where this version lets it draw: a landing the server
   * re-found draws at its line, one the revision left nowhere draws stale
   * at its captured line, and only a line the document no longer reaches
   * has no place at all.
   */
  const placements = $derived(placeThreads(held.threads, lines.length));
  const placed = $derived(placements.filter((held) => held.line !== null));
  const unplaceable = $derived(placements.filter((held) => held.line === null));

  /** The comment targets: rendered blocks for markdown, lines for text. */
  const rows = $derived(held.kind === 'text' ? rowsOf(text, 'text') : rowsOf(text, 'markdown'));

  /**
   * The drift is written back where it is drawn: a diff's overlay ages a
   * drifted code thread as it re-anchors it, and this viewer is the
   * overlay's counterpart for a document, so the review record reads what
   * the reader sees. A thread the landing MOVED is not drift - it still
   * sits at its line - so only a stale one ages. Resolved and
   * already-outdated comments are left alone.
   */
  $effect(() => {
    if (!held.reviewable || held.path === null) return;
    for (const held of placements) {
      if (!held.stale) continue;
      const thread = held.thread;
      // **A read with no landing is an older server, not drift.** Aging every
      // thread on one would write Outdated into the store for a document the
      // server never placed, so only a landing that is present ages.
      if (thread.resolution === null) continue;
      if (thread.status !== 'open' && thread.status !== 'addressed') continue;
      void connection
        ?.dispatch(statusCommand(slot, scope, thread.id, 'Outdated'))
        ?.catch((why: unknown) => {
          report('a comment could not be aged', why);
        });
    }
  });

  /**
   * The comment target a composer is open on: the rendered block (today's
   * whole-section comment) or one source line from the gutter. The line is
   * the anchor; `target` is what decides which lines the comment captures.
   */
  let composing = $state<{ row: number; line: number; target: 'block' | 'line' } | null>(null);
  let commentText = $state('');
  /** The block whose box began the current take, stamped when the take starts:
   *  it is what says the take goes with that box and lands only there. */
  let takeIn = $state<number | 'summary' | 'note' | 'edit' | null>(null);
  /** The exact line and target the open block composer was on, kept with the
   *  take so words that land after the box closed reopen where the reader
   *  was - a line comment must not come back as a section comment. */
  let takeLine = $state<number | null>(null);
  let takeTarget = $state<'block' | 'line'>('block');
  /** Whether the take being attempted began at the SUMMARY's mic - the box
   *  its words are owed to. */
  let pressedSummary = $state(false);
  /** Whether the take being attempted began at the ANSWER's note box. */
  let pressedNote = $state(false);
  /** Whether the take being attempted began at an EDIT box's mic. */
  let pressedEdit = $state(false);
  /** The review's own words, the round's description (Ved, 2026-10-17). */
  let summaryText = $state('');
  let summaryField = $state<HTMLElement | null>(null);
  /** The answer's optional note: filed with whichever answer is chosen and
   *  carried to the agent verbatim. */
  let noteText = $state('');
  let noteField = $state<HTMLElement | null>(null);

  /**
   * The box an EDIT opens, wherever a comment is drawn: the thread - or the
   * local draft - it changes, over the page's own take, so editing wears the
   * same mic, bar and panel as every other box here.
   */
  let edit = $state<ReplyBox & { draftId: string | null }>({
    thread: null,
    index: null,
    draftId: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    silent: null,
    panel: false,
    take: null,
    wire: null,
  });
  let editField = $state<HTMLElement | null>(null);

  /** The resolved comments the reader opened back up: collapsed by default,
   *  the same rule every surface keeps. */
  const opened = new SvelteSet<string>();

  /**
   * The seat's own record, read as an observer: the take's phase and the
   * notice a landing arrives on are the record's, and this page reads one for
   * exactly that - the words a take leaves reach a page only through it.
   */
  let session = $state<SessionRead>({ wire: null, refused: null, review: false });
  $effect(() => {
    return watchSession(connection, slot, false).subscribe(($next) => {
      session = $next;
    });
  });
  const composer = $derived(composerFrom(session.wire?.composer));

  /** The line a take this page could not start left behind, drawn in the
   *  block's own box - and the memory of a landed notice, which is what says
   *  whether a take's words are this page's to place at all. */
  let dictateLine = $state<TakeLine | null>(null);
  /** The page's own name: the take carries it and the memory reads it, one
   *  literal so the two ends cannot disagree. */
  const surface = 'doc' as const;
  const landing = new Landing(surface);

  /** The box's own field, so a landing, or a panel close, can put the
   *  keyboard back where the words are. */
  let field = $state<HTMLElement | null>(null);

  /**
   * The axes and input this seat dictates with, held as the composer's page
   * holds its own and written back to this machine's storage.
   */
  let axesEdit = $state<DictateAxes | null>(null);
  let deviceEdit = $state<string | null>(null);
  const key = $derived(boxKey(slot));
  const defaults = $derived(connection.settings()?.dictate ?? DEFAULT_AXES);
  const axes = $derived(axesEdit ?? axesFor(key, defaults));
  const device = $derived(deviceEdit ?? deviceFor(key));

  function setAxes(next: DictateAxes): void {
    axesEdit = next;
    rememberAxes(key, next);
  }

  function setDevice(next: string | null): void {
    deviceEdit = next;
    rememberDevice(key, next);
  }

  /**
   * The take this page owns: the same one the composer wears, addressed to
   * the seat this document belongs to, over the composer's own mic control.
   */
  const dictate = new Dictation({
    connection: () => connection,
    seat: () => slot,
    surface,
    shown: () => composer.take !== null,
    options: () => axesFor(boxKey(slot), defaults),
    device: () => deviceFor(boxKey(slot)),
    capture: openCapture,
    onStart: () => {
      // The take belongs to the box the press came from, which is the open
      // one: the mic is drawn only inside a box.
      takeIn = pressedEdit
        ? 'edit'
        : pressedSummary
          ? 'summary'
          : pressedNote
            ? 'note'
            : (composing?.row ?? null);
      takeLine = composing?.line ?? null;
      takeTarget = composing?.target ?? 'block';
      dictateLine = null;
      // The arm this take's landing waits on, set at the press, on the seat
      // the take belongs to, with the notice the record was already showing -
      // which the memory skips rather than spending the arm on.
      landing.arm(boxKey(slot), composer.notice);
    },
    onLine: (line) => {
      // No landing is coming for a take that drew a line: the arm goes with
      // the failure.
      dictateLine = line;
      landing.drop();
    },
    onRefused: (line) => {
      dictateLine = line;
    },
  });

  const rowPhase = $derived(takePhase(dictate, composer.take, slot));
  const rowWire = $derived(takeCounts(dictate, slot));
  /** What the row says when the device is feeding this page's capture nothing. */
  const rowSilent = $derived(takeSilenceLine(dictate, slot)?.text ?? null);

  /** What dictation means to this page, in the shape the shared boxes read. */
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
      return axes;
    },
    get device() {
      return device;
    },
    get defaults() {
      return defaults;
    },
    onaxes: setAxes,
    ondevice: setDevice,
  };

  /**
   * The thread cards' edit surface, over this page's one edit box: the
   * review page's `ThreadTurn` draws the act and the box per turn, and the
   * page keeps the state they share with the draft editor.
   */
  const turnEditor: TurnEditor = {
    box: edit,
    voice,
    begin: (thread, index, text) => {
      edit.thread = thread;
      edit.index = index;
      edit.draftId = null;
      edit.draft = text;
    },
    send: (thread, index, text) => {
      void editedThread(thread, index, text);
    },
    cancel: () => {
      edit.thread = null;
      edit.index = null;
      edit.draft = '';
    },
    press: () => {
      pressedEdit = true;
    },
    hold: () => {
      pressedEdit = true;
    },
    field: (el: HTMLElement | null) => {
      editField = el;
      el?.focus();
    },
  };

  /** The clock the thread turns' age chips read against. */
  const now = Date.now();

  /** What the edit box reads off the page's state, the way every box does. */
  $effect(() => {
    edit.take = composer.take;
    edit.wire = rowWire;
    edit.phase = takeIn === 'edit' ? rowPhase : null;
    edit.panel = dictate.panel && pressedEdit;
    edit.line = pressedEdit || takeIn === 'edit' ? blockLine : null;
  });

  /**
   * A landed take puts its words at the end of the box that began it, once.
   *
   * **A box that is not open, or no longer the take's own, has nowhere to put
   * them** - the reader closed the composer or moved it - and the words are
   * dropped rather than written where the reader is not. A notice this page
   * never watched is refused by the memory and must not touch the open box.
   *
   * **A carried notice is the record re-handing words to a box this page no
   * longer holds** - the page that began the take is gone, so the spot, the
   * thread or the turn they came from cannot be named. They are dropped, the
   * same rule a closed box gets, rather than guessed into another box. A
   * notice that names no surface is an older server and keeps its old road
   * to the composer.
   */
  $effect(() => {
    const words = landing.place(boxKey(slot), composer.notice, composer.take?.generation ?? null);
    if (words === null) return;
    if (
      composer.take === null &&
      composer.notice?.kind === 'landed' &&
      composer.notice.carried === true
    ) {
      return;
    }
    // Whose words these are is settled now, open box or not: the record hands
    // the same notice over on every write.
    landing.placed(composer.notice);
    if (takeIn === null) return;
    if (takeIn === 'edit') {
      edit.draft = joined(edit.draft, words);
      editField?.focus();
      return;
    }
    if (takeIn === 'summary') {
      summaryText = joined(summaryText, words);
      summaryField?.focus();
      return;
    }
    if (takeIn === 'note') {
      noteText = joined(noteText, words);
      noteField?.focus();
      return;
    }
    // The words were submitted: a box closed or moved between the release
    // and the landing is opened again at the row that began the take, where
    // the reader sees them, rather than the drop being silent.
    if (composing?.row !== takeIn) {
      composing = { row: takeIn, line: takeLine ?? takeIn, target: takeTarget };
    }
    commentText = joined(commentText, words);
    field?.focus();
  });

  /**
   * **A take goes with its box.** Closing the composer, or moving it to
   * another block, ends the take rather than leaving the microphone
   * capturing into a box nothing draws - whose words would land under
   * another block, one press from being filed against the wrong part.
   */
  $effect(() => {
    if (
      takeIn === null ||
      takeIn === 'summary' ||
      takeIn === 'note' ||
      takeIn === 'edit' ||
      composing?.row === takeIn
    )
      return;
    // A take already SUBMITTED is not this effect's to cancel: the server
    // would read the stop as an abandon and drop words the reader kept.
    const live = dictate.take;
    if (dictate.opening || (live !== null && !live.submitted)) {
      // The arm goes with the cancelled take: no notice is coming for it, and
      // a standing arm would ride the next one that does.
      landing.drop();
      dictate.act('cancel');
    }
  });

  // The line a take owes - this page's own about the attempt, or the record's
  // about a take the page watched - under the box it belongs to.
  const blockLine = $derived(
    dictateLine ?? noticeLine(composer.notice, landing.holds(composer.notice)),
  );

  /**
   * The box a key-begun take belongs to: the one the reader's caret is in,
   * else the open block composer, else the round's summary - the box the
   * exit row draws once there is a review to describe.
   */
  function pttBox(): 'composing' | 'summary' | 'note' | 'edit' | null {
    const active = document.activeElement;
    if (active !== null && active !== document.body) {
      if (active === field) return 'composing';
      if (active === summaryField) return 'summary';
      if (active === noteField) return 'note';
      if (active === editField) return 'edit';
    }
    if (edit.thread !== null || edit.draftId !== null) return 'edit';
    if (composing !== null) return 'composing';
    // A take already running keeps its box, so the key's release can end it.
    if (takeIn !== null) {
      return takeIn === 'summary'
        ? 'summary'
        : takeIn === 'note'
          ? 'note'
          : takeIn === 'edit'
            ? 'edit'
            : 'composing';
    }
    if (held.path !== null && drafts.length > 0) return 'summary';
    return held.path !== null && handoff !== null ? 'note' : null;
  }

  // The push-to-talk key, once for the page: a hold begins a take in the box
  // it belongs to, exactly as the composer's own box does.
  bindPushToTalk({
    dictate: () => dictate,
    offered: () => dictation && pttBox() !== null,
    bind: () => composer.bind,
    mode: () => composer.mode,
    // `offered` already asks for a drawn box, so a take can never land in a
    // hidden one here.
    machine: () => false,
    barLine: () => false,
    onBegin: () => {
      const at = pttBox();
      if (at === null) return;
      pressedSummary = at === 'summary';
      pressedNote = at === 'note';
      pressedEdit = at === 'edit';
      (at === 'summary'
        ? summaryField
        : at === 'note'
          ? noteField
          : at === 'edit'
            ? editField
            : field
      )?.focus();
    },
  });

  /**
   * The key that belongs to this box: a live take consumes Esc wherever the
   * focus is, and the settings panel is what the next Escape closes - the
   * same rule the composer's box applies to its own.
   */
  $effect(() => {
    const onDown = (event: KeyboardEvent): void => {
      if (event.key !== 'Escape') return;
      if (dictate.take !== null || dictate.opening || dictate.held) {
        event.preventDefault();
        event.stopPropagation();
        landing.drop();
        dictate.act('cancel');
        return;
      }
      if (dictate.panel) {
        event.preventDefault();
        event.stopPropagation();
        dictate.panel = false;
        // Back to the box the panel was opened off, whichever mic it was.
        (pressedSummary ? summaryField : pressedNote ? noteField : field)?.focus();
      }
    };
    window.addEventListener('keydown', onDown, true);
    return () => window.removeEventListener('keydown', onDown, true);
  });

  /** The door's one verb: approve or feedback where a call is parked, and
   *  plain done on a deep link, where there is nothing to answer. */
  type Finish = 'approve' | 'feedback' | 'done';
  let finishState = $state<
    | { kind: 'idle' }
    | { kind: 'sending'; answer: Finish }
    | { kind: 'done'; answer: Finish; number: number | null }
    | { kind: 'refused'; answer: Finish; why: string }
  >({ kind: 'idle' });
  /** The review this round filed, kept across a refused answer so a retry
   *  still names it. */
  let filedReview = $state<{ reviewId: string | null; number: number | null } | null>(null);

  /** Whether this page has been destroyed, which a pending return checks
   *  before it moves the app out from under whatever the reader is on. */
  let left = false;
  $effect(() => {
    return () => {
      left = true;
    };
  });

  /** Open the composer on a rendered block, or on one source line. A press
   *  on the block is today's whole-section comment; a press on a gutter
   *  number is a comment at that exact line. */
  function openComposer(row: DocRow, line: number, target: 'block' | 'line'): void {
    const same =
      composing?.row === row.line && composing.line === line && composing.target === target;
    composing = same ? null : { row: row.line, line, target };
    commentText = '';
    // The panel was opened off the box that is going; it goes with it.
    dictate.panel = false;
  }

  /** Close the composer, whatever target it was on. Cancel is a close, not a
   *  press on the block: a line comment must not reopen as a section one. */
  function closeComposer(): void {
    composing = null;
    commentText = '';
    dictate.panel = false;
  }

  /** Write one comment: held as a draft here, upserted to the store the
   *  moment it is written - the answer is what files it into a review. */
  function writeComment(row: DocRow): void {
    const written = commentText.trim();
    const at = composing;
    if (written === '' || held.path === null || at === null) return;
    const draft: DocDraft = {
      id: mintPromptId(),
      line: at.line,
      text: written,
      context: at.target === 'block' ? row.context : lineContext(lines, at.line),
    };
    addDraft(scope, draft);
    draftsVersion += 1;
    try {
      void connection
        ?.dispatch(upsertCommand(slot, scope, docThread(draft, held.path), written))
        ?.catch((why: unknown) => {
          report('a comment could not be stored', why);
        });
    } catch (why) {
      report('a comment could not be sent', why);
    }
    composing = null;
    commentText = '';
  }

  /** The page's refusal line for the edit and delete writes. */
  let said = $state<string | null>(null);

  /**
   * One write off the reader's own press, with the refusal drawn where the
   * press was: the edit and the delete answer through their reply, and a
   * `false` body is the store saying it did not take.
   */
  async function put(command: unknown): Promise<boolean> {
    const answer = connection.dispatch(command as never, slot);
    try {
      const body = await answer;
      said = null;
      if (body !== false) reread();
      return body !== false;
    } catch (why) {
      said = reasonOf(why);
      return false;
    }
  }

  /**
   * The document's own re-read: a write here rides the read the way a review
   * change rides the record on the other pages - without it the card keeps
   * the words the write replaced (measured live, Ved 2026-10-17, the edit
   * landing in the store and the page still reading the old text).
   */
  function reread(): void {
    connection.readDocument(slot, held.path ?? path, reviewable);
  }

  /** Replace one filed turn's words - the same upsert, identity kept. */
  async function editedThread(threadId: string, index: number, text: string): Promise<boolean> {
    const full = held.threads.find((thread) => thread.id === threadId);
    if (full === undefined) return false;
    const ok = await put(upsertCommand(slot, scope, withEditedTurn(full, index, text), text));
    if (!ok) return false;
    edit.thread = null;
    edit.index = null;
    edit.draft = '';
    return true;
  }

  /** Replace one local draft's words, its stored thread carried along. */
  function editedDraft(draftId: string, text: string): void {
    const next = editDraftText(scope, draftId, text);
    if (next === null) return;
    draftsVersion += 1;
    if (held.path !== null && held.contentHash !== null) {
      void connection
        ?.dispatch(upsertCommand(slot, scope, docThread(next, held.path), text))
        ?.catch((why: unknown) => {
          report('an edited comment could not be stored', why);
        });
      reread();
    }
    edit.draftId = null;
    edit.draft = '';
  }

  /** The retraction of a filed thread, wherever its card is drawn. */
  async function deletedThread(threadId: string): Promise<void> {
    await put(deleteCommand(slot, scope, threadId));
  }

  /** Open a resolved comment back up: the status write every surface's
   *  reopen rides. */
  async function reopenedThread(threadId: string): Promise<void> {
    await put(statusCommand(slot, scope, threadId, 'Open'));
  }

  /** The retraction of a local draft: off the shelf, and out of the store. */
  function deletedDraft(draftId: string): void {
    clearDrafts(scope, [draftId]);
    draftsVersion += 1;
    void connection?.dispatch(deleteCommand(slot, scope, draftId))?.catch((why: unknown) => {
      report('a deleted comment could not be removed from the store', why);
    });
    reread();
  }

  /** Send the open edit box: a filed thread's turn, or a local draft. */
  function sendEdit(text: string): void {
    if (edit.thread !== null) void editedThread(edit.thread, edit.index ?? 0, text);
    else if (edit.draftId !== null) editedDraft(edit.draftId, text);
  }

  /**
   * File the drafts as one review, through the same command a code review
   * rides. A refusal keeps the drafts and says why, and `null` means the
   * answer is not to be sent - nothing is answered until the review landed.
   */
  async function fileDrafts(answer: Finish): Promise<{
    reviewId: string | null;
    number: number | null;
  } | null> {
    // **A review already filed by a refused answer is still this round's.**
    // The drafts went with the filing, so a retry has nothing to submit; it
    // must answer with the review the first press filed rather than telling
    // the agent a review exists and handing it no id.
    if (drafts.length === 0 && filedReview !== null) return filedReview;
    let reviewId: string | null = null;
    let number: number | null = null;
    if (held.path !== null && drafts.length > 0) {
      const ids = drafts.map((draft) => draft.id);
      let filed: unknown;
      const summary = summaryText.trim();
      try {
        filed = await connection.dispatch(
          submitCommand(slot, scope, ids, summary === '' ? null : summary),
          slot,
        );
      } catch (why) {
        finishState = { kind: 'refused', answer, why: reasonOf(why) };
        return null;
      }
      if (typeof filed === 'string') {
        finishState = { kind: 'refused', answer, why: filed };
        return null;
      }
      if (filed === null || filed === undefined) {
        finishState = { kind: 'refused', answer, why: 'the review could not be filed' };
        return null;
      }
      clearDrafts(scope, ids);
      draftsVersion += 1;
      const held = filed as { id?: unknown; number?: unknown };
      reviewId = typeof held.id === 'string' ? held.id : null;
      number = typeof held.number === 'number' ? held.number : null;
      filedReview = { reviewId, number };
      // **The nudge the view this client replaced sent on its own drop**, so
      // the chat shows the review to the reader AND reaches the agent - one
      // line, not the comments - with the document's scope named. It stands
      // alone: a reload mounts the viewer with no hand-off to answer, so no
      // following answer can be promised, and it names no review tool.
      if (number !== null) {
        try {
          void connection.dispatch(
            {
              prompt_under: {
                key: slot,
                text: `Review #${number} ready on the document - scope ${scope}. Read it back under that scope, or hand the scope to your lead if you do not have the review tools.`,
                attachments: [],
                uuid: mintPromptId(),
                source: 'you',
              },
            },
            slot,
          );
        } catch {
          // The review is filed either way; a nudge that could not leave is
          // the reader's own line missing, never a lost review.
        }
      }
    }
    return { reviewId, number };
  }

  /**
   * Answer the parked hand-off: **approve** or **something is off**, each
   * with the note as written and the review the finish filed. Done is not
   * one of them - a reader who presses neither leaves the call parked, and
   * the agent learns nothing it could read as approval.
   */
  async function answer(outcome: 'approve' | 'feedback'): Promise<void> {
    finishState = { kind: 'sending', answer: outcome };
    const filed = await fileDrafts(outcome);
    if (filed === null) return;
    if (handoff !== null) {
      const note = noteText.trim();
      try {
        await connection.dispatch(
          {
            answer_document_hand_off: {
              key: slot,
              id: handoff,
              answer: {
                [outcome]: {
                  review_id: filed.reviewId,
                  note: note === '' ? null : note,
                },
              },
            },
          },
          slot,
        );
      } catch (why) {
        finishState = { kind: 'refused', answer: outcome, why: reasonOf(why) };
        return;
      }
    }
    filedReview = null;
    returnTo(outcome, filed.number, 1200);
  }

  /**
   * File and leave without answering: the one verb a deep link has.
   *
   * **The door opens even when the document never did.** A refused read
   * has no path and no drafts, so there is nothing to file - but the
   * reader is still owed the way back.
   */
  async function done(): Promise<void> {
    finishState = { kind: 'sending', answer: 'done' };
    const filed = await fileDrafts('done');
    if (filed === null) return;
    filedReview = null;
    returnTo('done', filed.number, 1200);
  }

  /** Leave the viewer for the session it came from, drawing the one-line
   *  confirmation first when `after` gives it a beat to be read. */
  function returnTo(outcome: Finish, number: number | null, after: number): void {
    finishState = { kind: 'done', answer: outcome, number };
    setTimeout(() => {
      if (!left) goTo({ name: 'session', slot });
    }, after);
  }

  /** The threads a row is the home of: their placed line is inside it. */
  function threadsUnder(row: DocRow): DocThreadPlacement[] {
    return placed.filter(
      (held) => held.line !== null && held.line >= row.line && held.line <= row.end,
    );
  }

  /** The source lines a rendered block spans, the gutter's own targets. */
  function rowLines(row: DocRow): number[] {
    const numbers: number[] = [];
    for (let line = row.line; line <= row.end; line += 1) numbers.push(line);
    return numbers;
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

<main class="wrap doc-page" class:wide={held.kind === 'pdf'}>
  {#if held.state === 'reading'}
    <p class="pending">Opening {path}...</p>
  {:else if held.state === 'refused'}
    <p class="doc-said">{held.refused}</p>
  {:else if held.bytes !== null && held.bytes.length === 0}
    <p class="doc-empty">The document is empty.</p>
    <!-- A read that says not reviewable draws read-only: the wire's own shape,
         and what a server from before the read answered the document's own
         reviewability answers a cold markdown or text open with. -->
  {:else if held.kind === 'markdown' && !held.reviewable}
    <article class="doc-body"><Prose {text} /></article>
  {:else if held.kind === 'text' && !held.reviewable}
    <article class="doc-body"><pre class="doc-text">{text}</pre></article>
  {:else if held.kind === 'pdf'}
    <article class="doc-body"><Pdf bytes={held.bytes ?? new Uint8Array()} /></article>
  {:else if held.kind === 'markdown' || held.kind === 'text'}
    <!-- A reviewable document: its rendered blocks, each one a comment
         target, and a source-line gutter beside it so a comment can land on
         one line. A press on the block is today's whole-section comment; a
         press on a gutter number is a comment at that exact line. -->
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
          class="doc-block prose"
          class:target={composing?.row === row.line}
          role="group"
          tabindex="0"
          aria-label="comment on this part of the document"
          onclick={(event) => {
            // A link inside the block is the link's, and a gutter number is
            // its own target: neither press also opens the block's composer.
            if (event.target instanceof Element && event.target.closest('a, button') !== null)
              return;
            openComposer(row, row.line, 'block');
          }}
          onkeydown={(event) => {
            // The keyboard half of the link rule: a reader tabbed onto a
            // link or a gutter number is on that control, and Enter/Space
            // there must reach it - the block answers only its own.
            if (event.target instanceof Element && event.target.closest('a, button') !== null)
              return;
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              openComposer(row, row.line, 'block');
            }
          }}
        >
          <!-- The gutter: one numbered target per source line the block
               spans, so a comment lands on the line the reader points at. -->
          <div class="doc-lines" role="group" aria-label="comment on a line">
            {#each rowLines(row) as line (line)}
              <button
                class="doc-lnum"
                class:target={composing?.row === row.line && composing.line === line}
                type="button"
                aria-label={`comment on line ${line}`}
                onclick={(event) => {
                  event.stopPropagation();
                  openComposer(row, line, 'line');
                }}>{line}</button
              >
            {/each}
          </div>
          <div class="doc-block-body">
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
        </div>
        {#each threadsUnder(row) as heldThread (heldThread.thread.id)}
          {@const thread = heldThread.thread}
          {#if statusName(thread.status) === 'resolved' && !opened.has(thread.id)}
            <ResolvedMarker
              first={thread.comments[0]?.text ?? ''}
              where={`${thread.anchor.path.split('/').pop() ?? ''} · line ${heldThread.line}`}
              onopen={() => opened.add(thread.id)}
            />
          {:else}
            <!-- Every turn the thread carries, so an agent's reply reads
                 beside the comment it answered after a revision. -->
            <div class="doc-note" class:doc-outdated={heldThread.stale}>
              <div class="doc-notehead">
                {#if heldThread.movedFrom !== null}
                  <span class="doc-movedfrom">moved from {heldThread.movedFrom}</span>
                {/if}
                {#if heldThread.stale}
                  <span class="doc-cc">outdated</span>
                {/if}
                <div class="rv-acts">
                  {#if statusName(thread.status) === 'resolved'}
                    <button
                      class="rv-a"
                      type="button"
                      onclick={() => void reopenedThread(thread.id)}>reopen</button
                    >
                    <button class="rv-a" type="button" onclick={() => opened.delete(thread.id)}
                      >collapse</button
                    >
                  {/if}
                  <ThreadDelete ondelete={() => void deletedThread(thread.id)} />
                </div>
              </div>
              {#each thread.comments as turn, index (index)}
                <ThreadTurn
                  {turn}
                  rounds={held.reviews}
                  {index}
                  {now}
                  thread={thread.id}
                  editor={turnEditor}
                />
              {/each}
            </div>
          {/if}
        {/each}
        {#each drafts.filter((draft) => draft.line >= row.line && draft.line <= row.end) as draft (draft.id)}
          <div class="doc-note doc-draft">
            <span class="doc-who">you</span>
            <span class="doc-where">line {draft.line}</span>
            <Prose text={draft.text} preserveLines />
            <span class="doc-cc">drafted</span>
            <div class="rv-acts">
              <button
                class="rv-a"
                type="button"
                onclick={() => {
                  edit.draftId = draft.id;
                  edit.thread = null;
                  edit.index = null;
                  edit.draft = draft.text;
                }}>edit</button
              >
              <ThreadDelete ondelete={() => deletedDraft(draft.id)} />
            </div>
          </div>
          {#if edit.draftId === draft.id}
            <WordsBox
              class="doc-note doc-editbox"
              rows={2}
              placeholder="the comment's words..."
              label="edit this comment"
              sendLabel="save"
              box={edit}
              {voice}
              onsend={sendEdit}
              oncancel={() => {
                edit.draftId = null;
                edit.draft = '';
              }}
              onpress={() => (pressedEdit = true)}
              onhold={() => (pressedEdit = true)}
              field={(el: HTMLElement | null) => {
                editField = el;
                el?.focus();
              }}
            />
          {/if}
        {/each}
        {#if composing?.row === row.line}
          <div class="doc-compose">
            <!-- **The block's box takes a take the way the composer's does**:
                 the bar takes the field's own line while a take runs, so
                 nothing is added and nothing moves as one starts or ends. -->
            <div class="line" class:takeline={rowPhase !== null}>
              {#if rowPhase !== null}
                <TakeCard
                  take={composer.take}
                  wire={rowWire}
                  phase={rowPhase}
                  oncancel={() => {
                    landing.drop();
                    dictate.act('cancel');
                  }}
                  spread
                  silent={rowSilent}
                />
              {:else}
                <Field
                  editor="nowhere"
                  placeholder="comment on this part..."
                  aria={{
                    label:
                      composing?.target === 'line'
                        ? `comment on line ${composing.line}`
                        : 'comment on this part of the document',
                  }}
                  bind:value={commentText}
                  onkeydown={(event: KeyboardEvent) => {
                    // **Enter comments**, as the button does, with the button's
                    // own guard: the same key sends in every box (Ved,
                    // 2026-10-15).
                    if (event.key !== 'Enter' || event.shiftKey) return;
                    event.preventDefault();
                    if (commentText.trim() !== '') writeComment(row);
                  }}
                  field={(el: HTMLElement | null) => {
                    field = el;
                  }}
                />
              {/if}
            </div>
            <div class="doc-compose-acts">
              <button
                class="doc-comment-btn"
                disabled={commentText.trim() === ''}
                onclick={() => writeComment(row)}>comment</button
              >
              <button class="doc-cancel-btn" onclick={closeComposer}>cancel</button>
              {#if dictation}
                <DictateControl
                  {dictate}
                  onpress={() => {
                    pressedSummary = false;
                    pressedNote = false;
                    pressedEdit = false;
                  }}
                  onhold={() => {
                    pressedSummary = false;
                    pressedNote = false;
                    pressedEdit = false;
                  }}
                />
              {/if}
            </div>
            {#if blockLine !== null}
              <div class="doc-dictateline {blockLine.tone}">{blockLine.text}</div>
            {/if}
            {#if dictate.panel && !pressedSummary && !pressedEdit}
              <DictationPanel
                {connection}
                {axes}
                {defaults}
                bind={composer.bind}
                mode={composer.mode}
                {device}
                onaxes={setAxes}
                ondevice={setDevice}
                onclose={() => {
                  dictate.panel = false;
                  // The keyboard goes back where Escape puts it, so closing by
                  // the button is not a dead end for a keyboard reader.
                  field?.focus();
                }}
              />
            {/if}
          </div>
        {/if}
      {/each}

      <!-- **Only a thread with nowhere to draw collects here.** One the
           revision could not re-find draws at its captured line inline,
           marked; this section is for the anchor whose line the document
           no longer reaches, and its sentence says why. -->
      {#if unplaceable.length > 0}
        <section class="doc-stale">
          <h2>cannot be placed in this version of the document</h2>
          {#each unplaceable as heldThread (heldThread.thread.id)}
            {@const thread = heldThread.thread}
            <div class="doc-note doc-quoted">
              {#if thread.anchor.context.length > 0}
                <div class="doc-quote">
                  {#each thread.anchor.context as captured, at (at)}<div>{captured}</div>{/each}
                </div>
              {/if}
              <p class="doc-why">{heldThread.why}</p>
              <div class="doc-notehead">
                <span class="doc-cc">outdated</span>
                <div class="rv-acts">
                  <ThreadDelete ondelete={() => void deletedThread(thread.id)} />
                </div>
              </div>
              {#each thread.comments as turn, index (index)}
                <ThreadTurn
                  {turn}
                  rounds={held.reviews}
                  {index}
                  {now}
                  thread={thread.id}
                  editor={turnEditor}
                />
              {/each}
            </div>
          {/each}
        </section>
      {/if}
    </article>
  {:else}
    <p class="doc-said">{otherLine(held.kind)}</p>
  {/if}

  {#if said !== null}
    <p class="doc-said">{said}</p>
  {/if}

  <!-- **The door is the hand-off's answer.** A parked call is answered by
       exactly two outcomes - approve, or something is off - each with an
       optional note, and pressing neither leaves the call parked. A deep
       link has no call to answer, so it keeps the one `done` that files the
       comments and returns. The door opens even when the document never
       did: a refused read still has an answer to give. -->
  {#if held.state !== 'reading'}
    <div class="doc-exit">
      {#if finishState.kind === 'done'}
        <p class="doc-left">
          {finishState.answer === 'approve'
            ? 'approved'
            : finishState.answer === 'feedback'
              ? 'sent back'
              : 'done'}{finishState.number === null
            ? ''
            : ` - review left (R${finishState.number})`}{handoff === null
            ? '.'
            : ' - the agent has it.'}
        </p>
      {:else}
        <!-- The round's description sits ABOVE the door, its own row and room
             to write - and it is the review's description only when there is
             a review to file (Ved, 2026-10-17: "the summary can be available
             already right above the finish review button"). -->
        {#if held.path !== null && drafts.length > 0}
          {@const summaryPhase = takeIn === 'summary' ? rowPhase : null}
          <div class="doc-wordline line" class:takeline={summaryPhase !== null}>
            {#if summaryPhase !== null}
              <TakeCard
                take={composer.take}
                wire={rowWire}
                phase={summaryPhase}
                oncancel={() => {
                  landing.drop();
                  dictate.act('cancel');
                }}
                spread
              />
            {:else}
              <Field
                editor="nowhere"
                class="rv-txarea"
                rows={3}
                placeholder="summary (optional)"
                aria={{ label: 'the round summary' }}
                bind:value={summaryText}
                field={(el: HTMLElement | null) => {
                  summaryField = el;
                }}
              />
            {/if}
          </div>
        {/if}
        {#if handoff !== null}
          <!-- The answer's note: one optional box, shared by both answers,
               carried to the agent verbatim with whichever is chosen. -->
          {@const notePhase = takeIn === 'note' ? rowPhase : null}
          <div class="doc-wordline line" class:takeline={notePhase !== null}>
            {#if notePhase !== null}
              <TakeCard
                take={composer.take}
                wire={rowWire}
                phase={notePhase}
                oncancel={() => {
                  landing.drop();
                  dictate.act('cancel');
                }}
                spread
                silent={rowSilent}
              />
            {:else}
              <Field
                editor="nowhere"
                class="rv-txarea"
                rows={2}
                placeholder="note (optional)"
                aria={{ label: 'a note to carry with your answer' }}
                bind:value={noteText}
                field={(el: HTMLElement | null) => {
                  noteField = el;
                }}
              />
            {/if}
          </div>
          <div class="rv-finishacts">
            <button
              class="doc-answer-btn"
              type="button"
              disabled={finishState.kind === 'sending'}
              onclick={() => void answer('approve')}
              >{finishState.kind === 'sending' && finishState.answer === 'approve'
                ? 'approving...'
                : 'approve'}</button
            >
            <button
              class="doc-answer-btn"
              type="button"
              disabled={finishState.kind === 'sending'}
              onclick={() => void answer('feedback')}
              >{finishState.kind === 'sending' && finishState.answer === 'feedback'
                ? 'sending...'
                : 'something is off'}</button
            >
            {#if dictation}
              <DictateControl
                {dictate}
                onpress={() => {
                  pressedSummary = false;
                  pressedNote = true;
                  pressedEdit = false;
                }}
                onhold={() => {
                  pressedSummary = false;
                  pressedNote = true;
                  pressedEdit = false;
                }}
              />
            {/if}
            {#if dictate.panel && pressedNote}
              <DictationPanel
                {connection}
                {axes}
                {defaults}
                bind={composer.bind}
                mode={composer.mode}
                {device}
                onaxes={setAxes}
                ondevice={setDevice}
                onclose={() => {
                  dictate.panel = false;
                  noteField?.focus();
                }}
              />
            {/if}
          </div>
        {:else}
          <div class="rv-finishacts">
            <button
              class="doc-finish-btn"
              disabled={finishState.kind === 'sending'}
              onclick={() => void done()}
              >done{drafts.length > 0 ? ` (${drafts.length})` : ''}</button
            >
            <!-- The mic lives with the box, not with the door: it draws only
                 when there is a summary to dictate into. -->
            {#if dictation && held.path !== null && drafts.length > 0}
              <DictateControl
                {dictate}
                onpress={() => {
                  pressedSummary = true;
                  pressedNote = false;
                  pressedEdit = false;
                }}
                onhold={() => {
                  pressedSummary = true;
                  pressedNote = false;
                  pressedEdit = false;
                }}
              />
            {/if}
            {#if dictate.panel && pressedSummary}
              <DictationPanel
                {connection}
                {axes}
                {defaults}
                bind={composer.bind}
                mode={composer.mode}
                {device}
                onaxes={setAxes}
                ondevice={setDevice}
                onclose={() => {
                  dictate.panel = false;
                  summaryField?.focus();
                }}
              />
            {/if}
          </div>
        {/if}
        {#if finishState.kind === 'refused'}
          <p class="doc-said">{finishState.why}</p>
        {/if}
      {/if}
    </div>
  {/if}
</main>

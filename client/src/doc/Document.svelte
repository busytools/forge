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
  import type { ReplyBox, Voice } from '../review/boxes';
  import ResolvedMarker from '../review/ResolvedMarker.svelte';
  import ThreadDelete from '../review/ThreadDelete.svelte';
  import { authorName, statusName } from '../review/view';
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
    rowsOf,
    placedThreads,
    threadsAgainstAnotherVersion,
    type DocDraft,
    type DocRow,
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
  /** The block whose box began the current take, stamped when the take starts:
   *  it is what says the take goes with that box and lands only there. */
  let takeIn = $state<number | 'summary' | 'edit' | null>(null);
  /** Whether the take being attempted began at the SUMMARY's mic - the box
   *  its words are owed to. */
  let pressedSummary = $state(false);
  /** Whether the take being attempted began at an EDIT box's mic. */
  let pressedEdit = $state(false);
  /** The review's own words, the round's description (Ved, 2026-10-17). */
  let summaryText = $state('');
  let summaryField = $state<HTMLElement | null>(null);

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
  let session = $state<SessionRead>({ wire: null, refused: null });
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
  const landing = new Landing();

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
    shown: () => composer.take !== null,
    options: () => axesFor(boxKey(slot), defaults),
    device: () => deviceFor(boxKey(slot)),
    capture: openCapture,
    onStart: () => {
      // The take belongs to the box the press came from, which is the open
      // one: the mic is drawn only inside a box.
      takeIn = pressedEdit ? 'edit' : pressedSummary ? 'summary' : composing;
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
   * them** - the reader closed the composer or moved it, or the notice is one
   * this page never watched (which the memory refuses) - and the words are
   * dropped rather than written where the reader is not. The box takes the
   * keyboard with them, the rule the composer's own landing keeps.
   */
  $effect(() => {
    const words = landing.place(boxKey(slot), composer.notice);
    // A take this page never began (a second dictating client on the seat)
    // places nothing - and must not touch the box the reader has open.
    if (words === null) return;
    // **The seat's carried words are the composer's to place, not this page's**
    // (#1360): a document box exists only while a take of THIS page's is in
    // flight - read off the record, not off the last box the page began -
    // so the record's re-hand must not open one. The memory stays unmarked
    // and the composer's own box takes them.
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
    // The words were submitted: a box closed or moved between the release
    // and the landing is opened again at the row that began the take, where
    // the reader sees them, rather than the drop being silent.
    if (composing !== takeIn) composing = takeIn;
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
    if (takeIn === null || takeIn === 'summary' || takeIn === 'edit' || composing === takeIn)
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
  function pttBox(): 'composing' | 'summary' | 'edit' | null {
    const active = document.activeElement;
    if (active !== null && active !== document.body) {
      if (active === field) return 'composing';
      if (active === summaryField) return 'summary';
      if (active === editField) return 'edit';
    }
    if (edit.thread !== null || edit.draftId !== null) return 'edit';
    if (composing !== null) return 'composing';
    // A take already running keeps its box, so the key's release can end it.
    if (takeIn !== null) {
      return takeIn === 'summary' ? 'summary' : takeIn === 'edit' ? 'edit' : 'composing';
    }
    return held.path !== null && drafts.length > 0 ? 'summary' : null;
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
      pressedSummary = at === 'summary';
      pressedEdit = at === 'edit';
      (at === 'summary' ? summaryField : at === 'edit' ? editField : field)?.focus();
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
        (pressedSummary ? summaryField : field)?.focus();
      }
    };
    window.addEventListener('keydown', onDown, true);
    return () => window.removeEventListener('keydown', onDown, true);
  });

  let finishState = $state<
    | { kind: 'idle' | 'busy' }
    | { kind: 'done'; number: number | null }
    | { kind: 'refused'; why: string }
  >({ kind: 'idle' });

  /** Whether this page has been destroyed, which a pending return checks
   *  before it moves the app out from under whatever the reader is on. */
  let left = false;
  $effect(() => {
    return () => {
      left = true;
    };
  });

  function openComposer(row: DocRow): void {
    composing = composing === row.line ? null : row.line;
    commentText = '';
    // The panel was opened off the box that is going; it goes with it.
    dictate.panel = false;
  }

  /** Write one comment: held as a draft here, upserted to the store the
   *  moment it is written - the done is what files it into a review. */
  function writeComment(row: DocRow): void {
    const written = commentText.trim();
    if (written === '' || held.path === null || held.contentHash === null) return;
    const draft: DocDraft = {
      id: mintPromptId(),
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

  /** The page's refusal line for the edit and delete writes. */
  let said = $state<string | null>(null);

  /**
   * One write off the reader's own press, with the refusal drawn where the
   * press was: the edit and the delete answer through their reply, and a
   * `false` body is the store saying it did not take.
   */
  async function put(command: unknown): Promise<boolean> {
    const answer = connection.dispatch(command as never, slot);
    if (answer === null) {
      said = 'the connection is down';
      return false;
    }
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
    const ok = await put(upsertCommand(slot, scope, withEditedTurn(full, index, text)));
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
        ?.dispatch(upsertCommand(slot, scope, docThread(next, held.path, held.contentHash)))
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
   * The reader is done: file the drafts as one review (through the same
   * command a code review rides), then release the parked hand-off with
   * what was filed - a refusal keeps the drafts and says why, and nothing
   * is answered until the review actually landed.
   *
   * **The door opens even when the document never did.** A refused read
   * has no path and no drafts, so there is nothing to file - but the
   * reader is still owed the way back, and a parked hand-off still needs
   * its answer: the guard that skips the FILING must not skip the return.
   */
  async function done(): Promise<void> {
    finishState = { kind: 'busy' };
    let reviewId: string | null = null;
    let number: number | null = null;
    if (held.path !== null && drafts.length > 0) {
      const ids = drafts.map((draft) => draft.id);
      let answer: unknown;
      const summary = summaryText.trim();
      try {
        answer = await connection.dispatch(
          submitCommand(slot, scope, ids, summary === '' ? null : summary),
          slot,
        );
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
      // **The nudge the view this client replaced sent on its own drop**, so
      // the chat shows the review to the reader AND reaches the agent - one
      // line, not the comments - with the document's scope named, which is
      // how the agent reads it back.
      if (number !== null) {
        try {
          void connection.dispatch(
            {
              prompt_under: {
                key: slot,
                text: `Review #${number} ready on the document - address it via the review MCP (\`review__list\` with the scope ${scope}).`,
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
    // **The done is the way back.** The reader came here from the session's
    // ask; the review and the nudge line are what they take with them, and
    // the page returns where they were - the beat is so the confirmation
    // above is read before it goes.
    returnTo(number, 1200);
  }

  /** Leave the viewer for the session it came from, drawing the one-line
   *  confirmation first when `after` gives it a beat to be read. */
  function returnTo(number: number | null, after: number): void {
    finishState = { kind: 'done', number };
    setTimeout(() => {
      if (!left) goTo({ name: 'session', slot });
    }, after);
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
          onclick={(event) => {
            // A link inside the block is the link's: the block is the comment
            // target, and a click that meant the link must not also open the
            // composer under the navigation.
            if (event.target instanceof Element && event.target.closest('a') !== null) return;
            openComposer(row);
          }}
          onkeydown={(event) => {
            // The keyboard half of the link rule: a reader tabbed onto a
            // link inside the block is on the link, and Enter/Space there
            // must reach it - the block answers only its own.
            if (event.target instanceof Element && event.target.closest('a') !== null) return;
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
          {#if statusName(thread.status) === 'resolved' && !opened.has(thread.id)}
            <ResolvedMarker
              first={thread.comments[0]?.text ?? ''}
              where={`${thread.anchor.path.split('/').pop() ?? ''} · line ${thread.anchor.line}`}
              onopen={() => opened.add(thread.id)}
            />
          {:else}
            <div class="doc-note">
              <span class="doc-who"
                >{authorName(thread.comments[0]?.author ?? { kind: 'unknown', raw: null })}</span
              >
              <!-- The words go through the chat's renderer, the same
                 `Prose` the review page's turns draw with, so a comment's
                 marks and its author's own lines read here as they read
                 there. -->
              <Prose text={thread.comments[0]?.text ?? ''} preserveLines />
              <div class="rv-acts">
                {#if statusName(thread.status) === 'resolved'}
                  <button class="rv-a" type="button" onclick={() => void reopenedThread(thread.id)}
                    >reopen</button
                  >
                  <button class="rv-a" type="button" onclick={() => opened.delete(thread.id)}
                    >collapse</button
                  >
                {/if}
                <button
                  class="rv-a"
                  type="button"
                  onclick={() => {
                    edit.thread = thread.id;
                    edit.index = 0;
                    edit.draftId = null;
                    edit.draft = thread.comments[0]?.text ?? '';
                  }}>edit</button
                >
                <ThreadDelete ondelete={() => void deletedThread(thread.id)} />
              </div>
            </div>
          {/if}
          {#if edit.thread === thread.id}
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
                edit.thread = null;
                edit.index = null;
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
        {#each drafts.filter((draft) => draft.line === row.line) as draft (draft.id)}
          <div class="doc-note doc-draft">
            <span class="doc-who">you</span>
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
        {#if composing === row.line}
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
                  aria={{ label: 'comment on this part of the document' }}
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
              <button class="doc-cancel-btn" onclick={() => openComposer(row)}>cancel</button>
              {#if dictation}
                <DictateControl
                  {dictate}
                  onpress={() => {
                    pressedSummary = false;
                    pressedEdit = false;
                  }}
                  onhold={() => {
                    pressedSummary = false;
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
              <Prose text={thread.comments[0]?.text ?? ''} preserveLines />
              <div class="rv-acts">
                <button
                  class="rv-a"
                  type="button"
                  onclick={() => {
                    edit.thread = thread.id;
                    edit.index = 0;
                    edit.draftId = null;
                    edit.draft = thread.comments[0]?.text ?? '';
                  }}>edit</button
                >
                <ThreadDelete ondelete={() => void deletedThread(thread.id)} />
              </div>
            </div>
            {#if edit.thread === thread.id}
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
                  edit.thread = null;
                  edit.index = null;
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
        </section>
      {/if}
    </article>
  {:else}
    <p class="doc-said">{otherLine(held.kind)}</p>
  {/if}

  {#if said !== null}
    <p class="doc-said">{said}</p>
  {/if}

  <!-- **Every opened document carries ONE door**, drawn on the page itself
       rather than left to the browser's chrome: `done`. It files the
       comments and answers the parked hand-off when there is something to
       release - a PDF or a read-only document included, which is what
       gives those states an exit at all - and simply returns to the
       session when there is nothing (a deep link). One verb a reader
       learns once. -->
  {#if held.state !== 'reading'}
    <div class="doc-exit">
      {#if finishState.kind === 'done'}
        <p class="doc-left">
          done{finishState.number === null
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
        <div class="rv-finishacts">
          <button
            class="doc-finish-btn"
            disabled={finishState.kind === 'busy'}
            onclick={() => void done()}>done{drafts.length > 0 ? ` (${drafts.length})` : ''}</button
          >
          <!-- The mic lives with the box, not with the door: it draws only
               when there is a summary to dictate into. -->
          {#if dictation && held.path !== null && drafts.length > 0}
            <DictateControl
              {dictate}
              onpress={() => {
                pressedSummary = true;
                pressedEdit = false;
              }}
              onhold={() => {
                pressedSummary = true;
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
        {#if finishState.kind === 'refused'}
          <p class="doc-said">{finishState.why}</p>
        {/if}
      {/if}
    </div>
  {/if}
</main>

<script lang="ts">
  import { SvelteSet } from 'svelte/reactivity';
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
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import Field from '../composer/Field.svelte';
  import { Landing } from '../composer/landing';
  import TakeCard from '../composer/TakeCard.svelte';
  import { noticeLine, joined } from '../composer/view';
  import { hrefForSlot } from '../routes';
  import { DEFAULT_AXES, type DictateAxes } from '../session/wire';
  import { composerFrom } from '../composer/wire';
  import { watchSession, type SessionRead } from '../session/live';
  import type { ReviewThreadRecord, SessionRecord } from '../session/wire';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import Canvas from './Canvas.svelte';
  import type { CommentBox, ReplyBox } from './boxes';
  import Conversation from './Conversation.svelte';
  import {
    commentsInScope,
    latestRound,
    loopState,
    roundHasWork,
    roundsNewestFirst,
    threadsInRound,
  } from './view';
  import {
    finishRound,
    newThread,
    reopenThread,
    statusCommand,
    upsertCommand,
    withReply,
    type ComposedComment,
    type Sends,
    type SendOutcome,
  } from './write';

  /**
   * The review page: one seat's reviews, as the conversation they ran as and
   * as the diff they are anchored to.
   *
   * It subscribes the seat itself, as the models page subscribes its own
   * subject, and it answers nothing - the dock that answers a prompt lives in
   * a composer this page does not have.
   *
   * **The page writes.** A comment is captured where it is written (the
   * anchor's hash and context, off the lines this page draws), filed when the
   * round is sealed, and a reply or a resolve goes out as it is made - all
   * through the same command surface the view this client replaced wrote
   * with.
   *
   * **Its states are drawn here rather than left to a neighbour.** The read
   * arrives as one of: nothing yet (loading), a seat the server refused (the
   * server's own sentence), the two halves read or failed apart, or both read
   * and nothing filed. A half whose outcome this build cannot name is drawn
   * as itself, because a read that answered something is not a read that
   * failed and a page that drew neither would be silent about it.
   */
  let {
    connection,
    slot,
    dictation,
  }: {
    /** The socket this page reads and writes through. Never null: the shell
     *  mounts the page only with one open. */
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
  const reviews = $derived(record === null ? null : record.reviews);
  const threads = $derived(reviews?.threads.outcome === 'read' ? reviews.threads.value : []);
  const rounds = $derived(
    reviews?.reviews.outcome === 'read' ? roundsNewestFirst(reviews.reviews.value) : [],
  );
  const loop = $derived(loopState(threads));
  /** Whether both reads answered, which is what an empty copy needs to be
   *  true: a half that failed draws its own line and no "nothing filed". */
  const bothRead = $derived(
    reviews !== null && reviews.threads.outcome === 'read' && reviews.reviews.outcome === 'read',
  );

  /** Which half of the page is on screen. */
  let tab = $state<'conversation' | 'files'>('files');
  /** The diff scope: 0 is all changes, i+1 is commit i. */
  let scopeIdx = $state(0);
  let lastCommit = $state(1);
  let pickerOpen = $state(false);
  let pickerHi = $state(0);
  let mode = $state<'unified' | 'split'>('split');

  const branch = $derived(record?.work.branch ?? '');
  const commits = $derived(record?.git.ahead?.commits ?? []);
  const scopeCommit = $derived(scopeIdx === 0 ? null : (commits[scopeIdx - 1]?.sha ?? null));
  const diffLayer = $derived(record?.diff.branchAhead ?? { state: 'clean' as const });

  /** The threads this sitting has drafted or replied on, unfiled: what a
   *  `finish` seals into a round. */
  const drafts = new SvelteSet<string>();
  let busy = $state(false);
  let summary = $state('');
  /**
   * Whether the FILES half's finish row is open.
   *
   * **The files half draws the finish collapsed** (Ved, 2026-10-16): a
   * reader still in the diff gets one control - `finish review (N)` - a
   * press opens the page's own finish row where they are reading, and a
   * sealed round collapses it again. The conversation half always draws the
   * row, so this is only about the half that draws the diff.
   */
  let finishOpen = $state(false);
  /** A press opened the row, so the summary field owes the reader the caret. */
  let summaryFocus = false;
  /** The last write's refusal, drawn where the reader acted; cleared by the
   *  next write that lands. */
  let refusal = $state<string | null>(null);
  /**
   * The boxes this page's words can go in: the comment box on a diff line,
   * the reply box under a thread (either half draws the same one), and the
   * round's summary field.
   *
   * **The page holds them all**, because the take's words and its own line
   * have to reach the box that began the take - a draft a box held itself
   * would go with the row when the canvas rebuilds it.
   */
  type BoxName = 'comment' | 'reply' | 'summary';
  let box = $state<CommentBox>({
    spot: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    panel: false,
    take: null,
    wire: null,
  });
  let reply = $state<ReplyBox>({
    thread: null,
    draft: '',
    busy: false,
    line: null,
    phase: null,
    panel: false,
    take: null,
    wire: null,
  });
  let summaryLine = $state<TakeLine | null>(null);

  /**
   * Where a take's words belong: the box whose mic began it.
   *
   * Stamped when the take starts - the press's own report only NAMES a box,
   * and a press that refused, or one that ended a live take, must not move a
   * landing that belongs to where that take began.
   */
  let takeIn = $state<BoxName | null>(null);
  /** The spot and thread the take's box held when it began: a box that has
   *  MOVED since is no longer the box the take belongs to, the same way a
   *  closed one is not. */
  let takeSpot = $state<CommentBox['spot']>(null);
  let takeThread = $state<string | null>(null);
  /** Where the last mic was pressed, or the last key begun a take: the box a
   *  refusal, or a take's own line, is owed to. */
  let pressedIn = $state<BoxName | null>(null);
  /** Where a page line about a take is drawn: the box the attempt was made in. */
  let lineIn = $state<BoxName | null>(null);
  /** Which box's mic opened the settings, which is the box that anchors the panel. */
  let panelIn = $state<BoxName | null>(null);

  /** This page's memory of the seat's takes: the arm its own press set, and
   *  what it has placed. A landing is placed once, and the memory is the
   *  seat's - the record in hand can be another seat's between two reads. */
  const landing = new Landing();

  /** The boxes' own field elements, so a key or a panel close can put the
   *  keyboard back in the box that holds the take. */
  const elements: Record<BoxName, HTMLElement | null> = {
    comment: null,
    reply: null,
    summary: null,
  };

  /**
   * The line a take this page could not start left behind, and where it is
   * drawn - the box the attempt was made in, never the next one opened.
   */
  let dictateLine = $state<TakeLine | null>(null);

  /** What `forge.toml` set, which a take with no pick of its own records under. */
  const defaults = $derived(connection.settings()?.dictate ?? DEFAULT_AXES);

  /**
   * The axes and input this seat dictates with, held here rather than per box:
   * the seat is the door's key, every box on the page shares it, and a panel
   * opened from one box must not show another's stale copy.
   */
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

  /**
   * The seat's composer as the composer itself narrows it: the take a landing
   * is read off, the notice, and the key and mode the settings panel states.
   */
  const composer = $derived(composerFrom(record?.composer));

  /**
   * The take this page owns, the same one the composer wears: the microphone
   * is the client's own, so a take started here is addressed to this seat and
   * the server sees an ordinary take on it - the box's own mic and the words
   * are the client's halves of the bargain, and only the client knows which
   * box a landing belongs in.
   */
  const dictate = new Dictation({
    connection: () => connection,
    seat: () => slot,
    shown: () => composer.take !== null,
    options: () => axesFor(boxKey(slot), defaults),
    device: () => deviceFor(boxKey(slot)),
    capture: openCapture,
    onStart: () => {
      // A take is being attempted: whatever line the last attempt left is
      // stale, so a retry does not open under it - and the take belongs to
      // the box the press named, AT the spot it held then.
      takeIn = pressedIn;
      lineIn = pressedIn;
      takeSpot = box.spot;
      takeThread = reply.thread;
      dictateLine = null;
      // The arm this take's landing waits on, set at the press, on the seat
      // the take belongs to, with the notice the record was already showing -
      // which the memory skips rather than spending the arm on.
      landing.arm(boxKey(slot), composer.notice);
    },
    onLine: (line) => {
      // The take's own line belongs to the box it began in, and no landing is
      // coming for it: the arm goes with the failure.
      lineIn = takeIn;
      dictateLine = line;
      landing.drop();
    },
    onRefused: (line) => {
      // A refused press belongs to the box that was pressed.
      lineIn = pressedIn;
      dictateLine = line;
    },
  });

  /** What the take's row is doing, read the way every surface reads it. */
  const rowPhase = $derived(takePhase(dictate, composer.take, slot));
  const rowWire = $derived(takeCounts(dictate, slot));

  /**
   * What dictation means to this page, built once and handed to every box.
   *
   * **One stable object whose fields are getters**, because the canvas mounts
   * its boxes by hand and a mount reads its props once: a `$derived` object
   * here would freeze the axes and the device at the box's mount, and the
   * panel opened off that box would then show stale chips and merge each
   * click onto the snapshot (the row the reconciler keeps is never rebuilt -
   * the same rule `composerRow` states for every other live value).
   */
  const voice = {
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

  /**
   * One box's own take row, or `null` when the take is another box's: a take
   * begun at one mic draws its bar there and not at every box on the page.
   */
  function phaseIn(at: BoxName): RowPhase | null {
    return takeIn === at ? rowPhase : null;
  }

  /**
   * What the canvas's boxes read off their own state.
   *
   * **A mounted row reads its props once**, so everything a take moves has to
   * reach the box through the proxy the card already reads: the write in
   * flight (a button that never learned a write had started would file the
   * same comment twice), the record's take and this page's counts, and the two
   * per-box readings - what the box's take row says, and whether it is the box
   * the settings opened off.
   */
  $effect(() => {
    for (const at of ['comment', 'reply'] as const) {
      const held = at === 'comment' ? box : reply;
      held.busy = busy;
      held.take = composer.take;
      held.wire = rowWire;
      held.phase = phaseIn(at);
      held.panel = dictate.panel && panelIn === at;
    }
  });

  /**
   * A landed take puts its words in the box that began it, once.
   *
   * **A box that has gone is opened again rather than losing them.** The
   * words were submitted - the reader pressed finish, or the key went up -
   * so a box closed or moved between the release and the landing gets its
   * spot (or its thread) back and takes the words where the reader sees
   * them, instead of the drop being silent. A notice this page never watched
   * is still refused by the memory. The box takes the keyboard with them,
   * the rule the composer's own landing keeps: an immediate Enter sends what
   * just landed.
   */
  $effect(() => {
    const words = landing.place(boxKey(slot), composer.notice);
    if (words === null) return;
    // **The seat's carried words are the composer's to place, not this page's**
    // (#1360): a review box exists only while a take of THIS page's is in
    // flight - read off the record, not off the last box the page began,
    // which stays stamped after the take is over - so a notice this page
    // never watched must not open one. The memory is left unmarked, and the
    // composer's own box is where the record hands them.
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
    if (takeIn === 'comment') {
      if (!sameSpot(box.spot, takeSpot)) box.spot = takeSpot;
      if (box.spot === null) return;
      box.draft = joined(box.draft, words);
    } else if (takeIn === 'reply') {
      if (reply.thread === null || reply.thread !== takeThread) reply.thread = takeThread;
      if (reply.thread === null) return;
      reply.draft = joined(reply.draft, words);
    } else {
      summary = joined(summary, words);
    }
    elements[takeIn]?.focus();
  });

  /**
   * The line a take owes, drawn in the box it belongs to: this page's own
   * line goes to the box the attempt was made in, the record's to the box
   * the take began in - and to the comment box only when no take of this
   * page's is in play, where it has drawn since the box was the page's only
   * one. A record line hard-routed to the comment box would draw nothing at
   * all for a take that failed in the reply or the summary, whose box is not
   * the comment box and whose failure the reader would never see.
   */
  $effect(() => {
    const line = dictateLine ?? noticeLine(composer.notice, landing.holds(composer.notice));
    const at = dictateLine !== null ? lineIn : (takeIn ?? 'comment');
    box.line = at === 'comment' ? line : null;
    reply.line = at === 'reply' ? line : null;
    summaryLine = at === 'summary' ? line : null;
  });

  /**
   * **A take goes with its box.** The box that began a LIVE take going -
   * the reader's cancel, a move to another spot or thread, a scope change,
   * the canvas going with the tab - ends the take with it: leaving the
   * microphone capturing into a box nothing draws is a take whose bar is
   * drawn nowhere. Only a live take is cancelled here; one already submitted
   * carries its words to the landing, which opens the box again rather than
   * spending them.
   */
  $effect(() => {
    if (takeIn === null) return;
    const gone =
      takeIn === 'summary'
        ? !summaryDrawn()
        : takeIn === 'comment'
          ? box.spot === null || !sameSpot(box.spot, takeSpot) || tab !== 'files'
          : !replyDrawn() || reply.thread !== takeThread;
    // A take already SUBMITTED - draining the capture, or waiting on a
    // connection to send its stop - is not this effect's to cancel: the
    // server would read it as an abandon and drop words the reader kept.
    const live = dictate.take;
    if (gone && (dictate.opening || (live !== null && !live.submitted))) {
      // The arm goes with the cancelled take: no notice is coming for it, and
      // a standing arm would ride the next one that does.
      landing.drop();
      dictate.act('cancel');
    }
  });

  /** Whether two spots are the same place, by value: the object identity of a
   *  rebuilt row is not this test's business. */
  function sameSpot(at: CommentBox['spot'], held: CommentBox['spot']): boolean {
    return (
      at !== null &&
      held !== null &&
      at.path === held.path &&
      at.side === held.side &&
      at.line === held.line
    );
  }

  /**
   * Whether the round's summary box is drawn by the half that is up: always
   * on the conversation half, and on the files half only while its own
   * finish row is open - the box a take goes with.
   */
  function summaryDrawn(): boolean {
    return tab === 'conversation' || (tab === 'files' && finishOpen);
  }

  /**
   * Whether the reply box's thread is drawn by the half that is up: on the
   * files half the canvas closes the box when its row goes, and on the
   * conversation half a thread is drawn only while a round holds it - a
   * drafted thread is in no round, so no box exists for it there.
   */
  function replyDrawn(): boolean {
    if (reply.thread === null) return false;
    if (tab === 'files') return true;
    return rounds.some((round) =>
      threadsInRound(threads, round.id).some((thread) => thread.id === reply.thread),
    );
  }

  /**
   * The box a key-begun take belongs to: the one the reader is in, else the
   * one open, else the summary - the conversation half's box that is always
   * there. `null` when the page has a take with nowhere to land.
   *
   * **The canvas's boxes count only while the canvas is what draws them.**
   * A spot or an open thread outlives the files tab, and a take begun
   * against one the reader cannot see has no bar drawn anywhere.
   */
  function pttBox(): BoxName | null {
    const held = document.activeElement;
    if (held !== null && held !== document.body) {
      for (const at of ['comment', 'reply', 'summary'] as const) {
        if (elements[at] === held) return at;
      }
    }
    if (tab === 'files' && box.spot !== null) return 'comment';
    if (replyDrawn()) return 'reply';
    return summaryDrawn() ? 'summary' : null;
  }

  // The push-to-talk key, once for the page: the microphone is the client's
  // own and every box drives one take, so a registration per box would fire
  // the key once per open box. It is offered only where a take has somewhere
  // to land - a take whose words no box can take is a take the reader loses.
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

  /**
   * The key that belongs to this page's boxes: a live take consumes Esc
   * wherever the focus is, and the settings panel is what the next Escape
   * closes - the same rule the composer's box applies to its own.
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
        if (panelIn !== null) elements[panelIn]?.focus();
      }
    };
    window.addEventListener('keydown', onDown, true);
    return () => window.removeEventListener('keydown', onDown, true);
  });

  /**
   * Send one write and read its outcome.
   *
   * **The reply IS the outcome** for the two commands that answer through
   * one, so its body rides back to the caller - the store's own answer, or
   * the server's word for a refusal - and the finish reads the round out of
   * it. A dead socket is the one case that never resolves, and it is drawn
   * where the reader acted rather than swallowed: a refused submit that
   * cleared the drafts would take the words with it, and a comment counted
   * as filed before the server said so would mint an empty round later. The
   * caller adds the thread to the drafts only after this says the write
   * landed.
   */
  async function write(command: unknown): Promise<SendOutcome> {
    const open = connection;
    if (open === null) {
      refusal = 'the socket is not open';
      return { went: false, body: null };
    }
    if (branch === '') {
      refusal = 'this seat has no branch to file a review against yet';
      return { went: false, body: null };
    }
    busy = true;
    try {
      const body = await open.dispatch(command as never, slot);
      refusal = null;
      return { went: true, body };
    } catch (why) {
      refusal = why instanceof Error ? why.message : String(why);
      return { went: false, body: null };
    } finally {
      busy = false;
      // The reviews ride the record, so the write's own outcome arrives by
      // re-reading it - nothing pushes a review change.
      open.refresh({ session: slot });
    }
  }

  /** The canvas keeps its draft unless this says the write landed: a refusal
   *  that cleared the box would take the reader's words with it. */
  async function commented(at: ComposedComment): Promise<boolean> {
    const thread = newThread({
      ...at,
      baseRef: record?.git.defaultBranch ?? '',
      commit: scopeCommit,
    });
    const { went } = await write(upsertCommand(slot, branch, thread));
    if (went) drafts.add(thread.id);
    return went;
  }

  async function replied(thread: { id: string }, text: string): Promise<boolean> {
    const full = threads.find((held) => held.id === thread.id);
    if (full === undefined) return false;
    const { went } = await write(upsertCommand(slot, branch, withReply(full, text)));
    if (went) drafts.add(full.id);
    return went;
  }

  async function resolved(thread: { id: string }): Promise<void> {
    await write(statusCommand(slot, branch, thread.id, 'Resolved'));
  }

  /**
   * The page's two ways to the socket, as the writes take them: the write
   * whose outcome is drawn where the reader acted, and the raw dispatch a
   * nudge rides - a nudge is best-effort, because the round or the flip it
   * follows is filed either way.
   */
  const sends: Sends = {
    write,
    dispatch: (command) =>
      connection === null ? null : connection.dispatch(command as never, slot),
  };

  async function reopened(thread: { id: string }): Promise<void> {
    const full = threads.find((held) => held.id === thread.id);
    await reopenThread(
      sends,
      slot,
      branch,
      thread.id,
      full === undefined ? null : latestRound(rounds, full),
    );
  }

  /** The files half's own finish: the row opens, the field takes the caret. */
  function openFinish(): void {
    finishOpen = true;
    summaryFocus = true;
  }

  async function finish(): Promise<void> {
    if (drafts.size === 0) return;
    const sealed = await finishRound(
      sends,
      slot,
      branch,
      [...drafts],
      summary.trim() === '' ? null : summary.trim(),
      roundHasWork(threads, drafts),
    );
    // The words survive a refusal: the reader's round is still theirs to
    // send once whatever refused it is fixed.
    if (sealed) {
      drafts.clear();
      summary = '';
      // The files half's row collapses with the round: the reader is back to
      // the diff with nothing of the round open.
      finishOpen = false;
      summaryFocus = false;
      // **Done closes the round** (Ved, 2026-10-15): the boxes, the summary's
      // panel and any take line go with it, so the page is back to the diff
      // with nothing of the round left open.
      box.spot = null;
      box.line = null;
      box.draft = '';
      reply.thread = null;
      reply.line = null;
      reply.draft = '';
      dictateLine = null;
      lineIn = null;
      pressedIn = null;
      panelIn = null;
      dictate.panel = false;
    }
  }

  /** The scope picker's rows, all changes first. */
  const choices = $derived([
    { idx: 0, label: 'All changes', sub: `${commits.length} commits` },
    ...commits.map((commit, i) => ({
      idx: i + 1,
      label: commit.subject,
      sub: commit.sha.slice(0, 8),
    })),
  ]);

  function choose(idx: number): void {
    scopeIdx = idx;
    if (idx > 0) lastCommit = idx;
    pickerOpen = false;
    // The stepper's place is not the new scope's: its comments are others.
    commentAt = -1;
  }

  /**
   * One scope along, CYCLING: the last commit steps on to all changes, and
   * all changes back to the last commit, so a reader holding an arrow walks
   * the branch round rather than hitting a wall.
   */
  function stepScope(delta: number): void {
    const next = scopeIdx + delta;
    if (next < 0) choose(choices.length - 1);
    else if (next >= choices.length) choose(0);
    else choose(next);
  }

  /** The comments this scope draws, in the order the canvas draws them. */
  const comments = $derived(commentsInScope(threads, scopeCommit));
  /** Which of them the stepper is on, -1 for none. */
  let commentAt = $state(-1);
  /** The place the readout names: never past the end of a list that shrank
   *  under it - a re-read can drop the comment the reader was on. */
  const commentAtShown = $derived(
    commentAt < 0 || comments.length === 0 ? 0 : Math.min(commentAt, comments.length - 1) + 1,
  );
  /**
   * The comment the stepper last moved to, and the step's own number - a
   * repeat of the same target is a step all the same, and the canvas has to
   * hear it.
   */
  let focusComment = $state<{
    path: string;
    side: 'old' | 'new';
    line: number;
    seq: number;
  } | null>(null);
  let stepSeq = 0;

  /** One comment along, cycling both ways. */
  function stepComment(delta: number): void {
    if (comments.length === 0) return;
    const next = commentAt + delta;
    commentAt = next < 0 ? comments.length - 1 : next >= comments.length ? 0 : next;
    const held = comments[commentAt];
    if (held === undefined) return;
    stepSeq += 1;
    focusComment = {
      path: held.thread.anchor.path,
      side: held.thread.anchor.side === 'old' ? 'old' : 'new',
      line: held.line,
      seq: stepSeq,
    };
  }

  function toggleAll(): void {
    choose(scopeIdx === 0 ? lastCommit : 0);
  }

  /**
   * The diff half's keys, live only while it is on screen: the arrows (and
   * `[` / `]`) step one scope, cycling, shift+arrows step one comment,
   * cycling, `a` flips between all changes and the commit you were on, `j`
   * opens the picker. A keypress in a text field is the field's.
   */
  function onKey(event: KeyboardEvent): void {
    if (tab !== 'files') return;
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const typing =
      event.target instanceof HTMLElement && event.target.closest('input, textarea') !== null;
    if (typing) return;
    if (pickerOpen) {
      if (event.key === 'ArrowDown') pickerHi = Math.min(choices.length - 1, pickerHi + 1);
      else if (event.key === 'ArrowUp') pickerHi = Math.max(0, pickerHi - 1);
      else if (event.key === 'Enter') choose(choices[pickerHi]?.idx ?? 0);
      else if (event.key === 'Escape' || event.key === 'j') pickerOpen = false;
      else return;
      event.preventDefault();
      return;
    }
    // The arrows step commits and shift+arrows step comments - the pair the
    // keycap row under the tabs states.
    if (event.key === '[' || event.key === 'ArrowLeft') {
      if (event.shiftKey) stepComment(-1);
      else stepScope(-1);
    } else if (event.key === ']' || event.key === 'ArrowRight') {
      if (event.shiftKey) stepComment(1);
      else stepScope(1);
    } else if (event.key === 'a') toggleAll();
    else if (event.key === 'j') {
      pickerHi = scopeIdx;
      pickerOpen = true;
    } else return;
    event.preventDefault();
  }

  $effect(() => {
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  /** The clock the ages read against; ticking keeps a sitting page honest. */
  let now = $state(Date.now());
  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(tick);
  });
</script>

<!-- The round's finish, drawn by both halves: the conversation half's Finish
     section, and the files half's own row, where it opens on a press and
     collapses again once the round is sealed. One snippet, because the round,
     the field and the write are the page's own either way. -->
{#snippet roundFinish()}
  <div class="rv-finishstack">
    <!-- The round's own words take a take the way every box does: the bar in
         the field's place while it runs, and the words at the end of what is
         written. -->
    <div class="rv-wordline line" class:takeline={phaseIn('summary') !== null}>
      {#if phaseIn('summary') !== null}
        <TakeCard
          take={composer.take}
          wire={rowWire}
          phase={phaseIn('summary') ?? 'recording'}
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
          rows={4}
          placeholder="summary (optional)"
          aria={{ label: 'the round summary' }}
          bind:value={summary}
          onkeydown={(event: KeyboardEvent) => {
            // **Enter finishes the review**, as its own button does: the same
            // key sends everywhere, and the same guard keeps a round with
            // nothing filed from sealing (Ved, 2026-10-15).
            if (event.key !== 'Enter' || event.shiftKey) return;
            event.preventDefault();
            if (!busy && drafts.size > 0) void finish();
          }}
          field={(el: HTMLElement | null) => {
            elements.summary = el;
            // A finish a press opened owes the field the caret, the same way
            // a comment box a click opened does.
            if (el !== null && summaryFocus) {
              summaryFocus = false;
              el.focus();
            }
          }}
        />
      {/if}
    </div>
    <div class="rv-finishacts">
      <button
        type="button"
        class="rv-finish"
        disabled={busy || drafts.size === 0}
        onclick={() => void finish()}
      >
        finish review
        {#if drafts.size > 0}({drafts.size}){/if}
      </button>
      {#if dictation}
        <DictateControl
          {dictate}
          onpress={() => (pressedIn = 'summary')}
          onhold={() => (panelIn = 'summary')}
        />
      {/if}
      {#if dictate.panel && panelIn === 'summary'}
        <div>
          <DictationPanel
            connection={voice.connection}
            axes={voice.axes}
            defaults={voice.defaults}
            bind={voice.bind}
            mode={voice.mode}
            device={voice.device}
            onaxes={voice.onaxes}
            ondevice={voice.ondevice}
            onclose={() => {
              dictate.panel = false;
              elements.summary?.focus();
            }}
          />
        </div>
      {/if}
    </div>
    {#if summaryLine !== null}
      <div class="rv-dictateline {summaryLine.tone}">{summaryLine.text}</div>
    {/if}
  </div>
{/snippet}

<main class="wrap review">
  <header class="top">
    <a class="back" href={hrefForSlot(slot)}>&#8249; {slot.label}</a>
    <h1>Review</h1>
    {#if record !== null}
      <span class="facts">
        {record.work.branch ?? ''}
        {#if rounds.length}
          <span class="sep">&#183;</span>
          {rounds.length}
          {rounds.length === 1 ? 'round' : 'rounds'}
        {/if}
      </span>
      <span class="loop" class:needs={loop.waiting > 0}>
        {#if loop.waiting > 0}
          {loop.waiting} {loop.waiting === 1 ? 'reply' : 'replies'} to read
        {:else}
          nothing waiting on you
        {/if}
        {#if loop.open > 0}<span class="sep">&#183;</span> {loop.open} open{/if}
      </span>
    {/if}
  </header>

  {#if read.refused !== null}
    <p class="pending">This forge would not answer for the seat: {read.refused}</p>
  {:else if reviews === null}
    <p class="pending">Reading the seat...</p>
  {:else}
    {#if reviews.threads.outcome === 'failed'}
      <p class="half bad">The threads could not be read: {reviews.threads.value}</p>
    {:else if reviews.threads.outcome === 'unknown'}
      <p class="half">
        The threads read answered <code>{String(reviews.threads.value)}</code>, which this build
        cannot name.
      </p>
    {/if}
    {#if reviews.reviews.outcome === 'failed'}
      <p class="half bad">The rounds could not be read: {reviews.reviews.value}</p>
    {:else if reviews.reviews.outcome === 'unknown'}
      <p class="half">
        The rounds read answered <code>{String(reviews.reviews.value)}</code>, which this build
        cannot name.
      </p>
    {/if}

    <nav class="rv-tabs" aria-label="review sections">
      <button
        type="button"
        class:on={tab === 'conversation'}
        onclick={() => (tab = 'conversation')}
      >
        Conversation
      </button>
      <button type="button" class:on={tab === 'files'} onclick={() => (tab = 'files')}>
        Files changed <span class="rv-tn">{commits.length}</span>
      </button>
      {#if tab === 'files'}
        <span class="rv-scope">
          <button
            type="button"
            class="rv-scope-btn"
            aria-haspopup="listbox"
            aria-expanded={pickerOpen}
            onclick={() => {
              pickerHi = scopeIdx;
              pickerOpen = !pickerOpen;
            }}
          >
            <span class="rv-scope-lb">Scope</span>
            <span class="rv-scope-now">
              {scopeCommit === null ? 'All changes' : (scopeCommit.slice(0, 8) ?? '')}
            </span>
            <span class="rv-scope-sub">
              {scopeCommit === null
                ? `${commits.length} commits`
                : (commits[scopeIdx - 1]?.subject ?? '')}
            </span>
          </button>
          <span class="rv-step">
            <button
              type="button"
              class="rv-step-btn"
              aria-label="previous scope"
              disabled={choices.length <= 1}
              onclick={() => stepScope(-1)}>&#8249;</button
            >
            <button
              type="button"
              class="rv-step-btn"
              aria-label="next scope"
              disabled={choices.length <= 1}
              onclick={() => stepScope(1)}>&#8250;</button
            >
          </span>
          {#if pickerOpen}
            <div class="rv-menu" role="listbox" aria-label="diff scope">
              {#each choices as choice (choice.idx)}
                <button
                  type="button"
                  class="rv-menu-it"
                  class:hi={pickerHi === choice.idx}
                  class:on={choice.idx === scopeIdx}
                  role="option"
                  aria-selected={choice.idx === scopeIdx}
                  onclick={() => choose(choice.idx)}
                >
                  <span class="rv-menu-label">{choice.label}</span>
                  <span class="rv-menu-sub">{choice.sub}</span>
                </button>
              {/each}
            </div>
          {/if}
        </span>
        <span class="rv-mode rv-viewmode">
          <button type="button" class:on={mode === 'unified'} onclick={() => (mode = 'unified')}>
            unified
          </button>
          <button type="button" class:on={mode === 'split'} onclick={() => (mode = 'split')}>
            split
          </button>
        </span>
        <!-- The comments' own stepper, beside the scope's: the two things a
             reader walks in a diff, each with a button a finger can press -
             a key alone is no door on a touch screen (rule 22). -->
        <span class="rv-step rv-cstep">
          <button
            type="button"
            class="rv-step-btn"
            aria-label="previous comment"
            disabled={comments.length === 0}
            onclick={() => stepComment(-1)}>&#8249;</button
          >
          <span class="rv-cstep-n" aria-live="polite">{commentAtShown}/{comments.length}</span>
          <button
            type="button"
            class="rv-step-btn"
            aria-label="next comment"
            disabled={comments.length === 0}
            onclick={() => stepComment(1)}>&#8250;</button
          >
        </span>
      {/if}
    </nav>

    {#if tab === 'files'}
      <!-- The keys, where they act, in the reader's own line of sight: the
           arrows are what a hand reaches for, and the brackets the scope
           also accepts are not worth advertising beside them. -->
      <p class="rv-hint">
        <kbd>&#8592;</kbd><kbd>&#8594;</kbd> step commits
        <span class="sep">&#183;</span>
        <kbd>shift</kbd><kbd>&#8592;</kbd><kbd>&#8594;</kbd> step comments
        <span class="sep">&#183;</span>
        <kbd>a</kbd> all changes
        <span class="sep">&#183;</span>
        <kbd>j</kbd> pick a commit
        <span class="sep">&#183;</span>
        click any line to comment
      </p>
    {/if}

    {#if refusal !== null}
      <p class="rv-refusal">The write was refused: {refusal}</p>
    {/if}

    {#if tab === 'conversation'}
      {#snippet convFooter()}
        <section class="group">
          <h2>Finish</h2>
          <p class="rv-quiet">
            Comments written on the diff file into the next round when you finish the review.
          </p>
          {@render roundFinish()}
        </section>
      {/snippet}
      <!-- The conversation is one draw for both pages (`Conversation.svelte`):
           the branch's rounds here, the seat tree's on the files page. -->
      <Conversation
        {threads}
        {rounds}
        {drafts}
        {busy}
        {now}
        {voice}
        {reply}
        known={bothRead}
        onreply={replied}
        onresolve={(thread: ReviewThreadRecord) => void resolved(thread)}
        onreopen={(thread: ReviewThreadRecord) => void reopened(thread)}
        onpress={() => (pressedIn = 'reply')}
        onhold={() => (panelIn = 'reply')}
        onfield={(el: HTMLElement | null) => (elements.reply = el)}
        footer={convFooter}
      />
    {:else}
      <div class="rv-body rv-filesbody">
        <Canvas
          {connection}
          {slot}
          {scopeCommit}
          {mode}
          layer={diffLayer}
          {threads}
          reviews={reviews.reviews.outcome === 'read' ? reviews.reviews.value : []}
          {busy}
          {dictate}
          keysOff={pickerOpen}
          {focusComment}
          bind:box
          bind:reply
          {voice}
          onpress={(at: 'comment' | 'reply') => (pressedIn = at)}
          onhold={(at: 'comment' | 'reply') => (panelIn = at)}
          onelement={(at: 'comment' | 'reply', el: HTMLElement | null) => (elements[at] = el)}
          oncomment={commented}
          onreply={replied}
          onresolve={resolved}
          onreopen={reopened}
        />
      </div>
      <!-- The files half's own way to finish: the round's row, opened here
           by a press and collapsed again once it seals, so the reader never
           has to leave the diff to close a round. -->
      <div class="rv-finishbar">
        {#if finishOpen}
          {@render roundFinish()}
        {:else}
          <button
            type="button"
            class="rv-finish"
            disabled={busy || drafts.size === 0}
            onclick={openFinish}
          >
            finish review
            {#if drafts.size > 0}({drafts.size}){/if}
          </button>
        {/if}
      </div>
    {/if}
  {/if}
</main>

<style>
  /* The page's own rules, over the sheet's furniture: `wrap` and `top` are
     the app's, everything under `review` is this page's. */
  .review {
    display: flex;
    flex-direction: column;
    gap: 6px;
    /* The diff half scrolls inside the page rather than the page growing:
       the tree and the canvas each keep their own scroll, the way the app's
       own shell does. The inset is the shell's own, so the page wastes no
       more room at the edges than any other surface does. */
    height: 100dvh;
    overflow: hidden;
    padding: var(--ins);
  }
  /* The tab strip itself lives in the sheet now (the files page draws one
     too); what stays here is the scope control - one button naming the scope
     on screen, the list behind it, and a stepper beside it. */
  .rv-scope {
    position: relative;
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: auto;
  }
  .rv-scope-btn {
    display: flex;
    align-items: baseline;
    gap: 8px;
    max-width: 460px;
    background: var(--s1);
    border: 1px solid var(--line);
    border-radius: 8px;
    padding: 4px 10px;
    cursor: pointer;
    font: inherit;
    color: inherit;
  }
  .rv-scope-btn:hover {
    border-color: var(--ctl);
  }
  .rv-scope-lb {
    font-size: var(--fs-label);
    font-weight: 700;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--dim);
    flex: none;
  }
  .rv-scope-now {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--text);
    flex: none;
  }
  .rv-scope-sub {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .rv-step {
    display: flex;
  }
  .rv-step-btn {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--muted);
    background: none;
    border: 1px solid var(--line);
    padding: 3px 9px;
    cursor: pointer;
  }
  .rv-step-btn:first-child {
    border-radius: 8px 0 0 8px;
  }
  .rv-step-btn:last-child {
    border-radius: 0 8px 8px 0;
    border-left: 0;
  }
  .rv-step-btn:hover {
    color: var(--text);
    border-color: var(--ctl);
  }
  .rv-step-btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .rv-hint {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 7px;
    padding: 2px 2px 10px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
  }
  /* The comment stepper's own count, the cell between its arrows. */
  .rv-cstep-n {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--muted);
    border: 1px solid var(--line);
    border-left: 0;
    padding: 3px 8px;
    white-space: nowrap;
  }
  .rv-cstep {
    flex: none;
  }
  .rv-hint .sep {
    color: var(--line);
    margin: 0 2px;
  }
  .rv-hint kbd {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--muted);
    background: var(--s1);
    border: 1px solid var(--line);
    border-bottom-width: 2px;
    border-radius: 5px;
    padding: 1px 6px;
    line-height: 1.4;
  }
  .rv-refusal {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--bad);
    padding: 2px 2px 8px;
  }
  /* Narrow: the scope control drops to its own line under the tabs rather
     than being clipped at the edge - it is the door to every commit, and a
     control a finger cannot reach is not there - and the page becomes one
     scrolling column, because a phone has no room for panes each holding
     their own scroll. */
  @media (max-width: 980px) {
    .review {
      height: auto;
      overflow: visible;
    }
    .rv-tabs {
      flex-wrap: wrap;
    }
    .rv-scope {
      margin-left: 0;
      width: 100%;
    }
    .rv-hint {
      display: none;
    }
    /* The one column scrolls far past a refusal written above it - the
       reader 231k px down saw nothing at all - so at this width the line
       rides the top of the viewport instead. */
    .rv-refusal {
      position: sticky;
      top: 0;
      z-index: 5;
      background: var(--bg);
      border-bottom: 1px solid var(--line);
      padding: 6px 2px;
    }
  }
  .rv-menu {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    z-index: 40;
    width: 460px;
    max-width: calc(100vw - 40px);
    max-height: 340px;
    overflow-y: auto;
    background: var(--s2);
    outline: 1px solid var(--line);
    outline-offset: -1px;
    border-radius: 11px;
    box-shadow: 0 14px 38px rgba(0, 0, 0, 0.5);
    padding: 4px;
  }
  .rv-menu-it {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 62px;
    gap: 8px;
    align-items: baseline;
    width: 100%;
    text-align: left;
    background: none;
    border: 0;
    border-radius: 8px;
    padding: 6px 9px;
    cursor: pointer;
    font: inherit;
  }
  .rv-menu-it.hi {
    background: var(--s3);
  }
  .rv-menu-it.on .rv-menu-label {
    color: var(--accent);
  }
  .rv-menu-label {
    font-size: var(--fs-data);
    color: var(--text);
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rv-menu-sub {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    text-align: right;
  }
  .rv-mode {
    display: flex;
    border: 1px solid var(--line);
    border-radius: 99px;
    overflow: hidden;
  }
  .rv-mode button {
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    background: none;
    border: 0;
    padding: 3px 11px;
    cursor: pointer;
  }
  .rv-mode button + button {
    border-left: 1px solid var(--line);
  }
  .rv-mode button.on {
    color: var(--text);
    background: var(--s2);
  }
  .rv-mode button:hover {
    color: var(--muted);
  }
  .rv-viewmode {
    flex: none;
  }
  /* The finish stack itself - `.rv-finishstack`, `.rv-finishacts`, the
     `.rv-txarea` and `.rv-finish` - lives in the sheet now: the review
     page, the files rail and the document's exit all draw one shape (Ved,
     2026-10-17). What stays here is the page's own bar around it: the
     collapsed button right-aligned, the stack in its place once it opens. */
  .rv-finishbar {
    display: flex;
    align-items: center;
    gap: 9px;
    justify-content: flex-end;
    padding-top: 4px;
    flex: none;
  }
  .rv-finishbar .rv-finishstack {
    flex: 1;
    min-width: 0;
  }
  .top {
    align-items: baseline;
  }
  .back {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
    text-decoration: none;
  }
  .back:hover {
    color: var(--accent);
  }
  h1 {
    margin: 0;
    font-size: var(--fs-title);
    font-weight: 650;
    letter-spacing: -0.01em;
  }
  .facts {
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--dim);
  }
  .loop {
    margin-left: auto;
    font-family: var(--mono);
    font-size: var(--fs-data);
    color: var(--muted);
  }
  .loop.needs {
    color: var(--warn);
  }
  .sep {
    color: var(--line);
    margin: 0 7px;
  }

  .half {
    font-size: var(--fs-base);
    color: var(--muted);
    margin: 4px 0;
  }
  .half.bad {
    color: var(--bad);
  }

  .group {
    margin: 14px 0 8px;
  }
  .group h2 {
    display: flex;
    gap: 8px;
    align-items: baseline;
    margin: 0 0 6px;
    font-size: var(--fs-label);
    font-weight: 700;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--dim);
  }

  /* The turn's own rules live in the sheet (`ThreadTurn.svelte` draws the
     turn for both halves of the page, so its classes are the page's). */
</style>

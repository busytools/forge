<script lang="ts">
  import { untrack, type Snippet } from 'svelte';
  import { VList, type VListHandle } from 'virtua/svelte';
  import { heldReserve, sizes } from './row-sizes';

  import Icon from '../components/Icon.svelte';
  import { cannotPaint } from '../paint';
  import { noteConversationOpened, noteDrawn, type DrawnGeometry } from '../perf';
  import { MORE_TURNS, subjectKey } from '../protocol';
  import { scrollAsk } from '../session/scroll-ask';
  import type { Connection } from '../socket';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import { anchoredScroll, anchorAt, type Anchor, type RowBox } from './anchor';
  import { latestCompaction } from './compaction-jump';
  import {
    Chat,
    HISTORY_WINDOW_BYTES,
    NOTHING,
    beingWritten,
    headMoved,
    runningAt,
    type Conversation,
    type Turn as HeldTurn,
  } from './conversation';
  import { conversationsFor } from './conversations';
  import Echo from './Echo.svelte';
  import { echoes, ownWords } from './echoes.svelte';
  import { pinnedRow } from './pinnedRow.svelte';
  import Turn from './Turn.svelte';
  import { turnOfDispatch } from './dispatch-jump';
  import { callRow, reachableIds, reveal, subagents } from './subagents.svelte';
  import { stopsOf, turnFold, type TurnInfo } from './units';

  /**
   * The conversation: whole turns, virtualised.
   *
   * **The DOM holds what is on screen and the rest stays data**, which is what
   * lets a seat with a long history scroll without the page growing with it.
   *
   * **It opens at the latest turn.** The newest page is what the first ask
   * returns and the list is scrolled to its end once that page lands: a reader
   * arriving at a seat wants what was just said, not the beginning of a
   * conversation that may be days old.
   *
   * **It never regroups.** The turns arrive decided, one row each; a component
   * that derived a group from what it happened to hold is the defect this
   * whole design exists to prevent.
   */
  let {
    slot,
    connection,
    waking = false,
    spawning = false,
    queue = undefined,
    onloaded = () => {},
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The roster holds no session for this seat, which is its own state. */
    waking?: boolean;
    /** Whether the core is bringing the seat up, the waking line's other half. */
    spawning?: boolean;
    /** The waiting prompts, drawn above the pinned row: the page owns the
     *  queue's data, and its place is the column's foot. */
    queue?: Snippet | undefined;
    /** Report of the column's own `loaded` flag, which the band's read wait
     *  is drawn from. */
    onloaded?: (loaded: boolean) => void;
  } = $props();

  /** How near the top the reader has to be before the turns above are asked for. */
  const REACH = 400;
  /**
   * How much of the room a page above aims to fill.
   *
   * **A page is sized by height, not by a turn count** (#2398): a landing's
   * cost is the page's real height - the compensation rewrites the scroll by
   * it and the reader re-traverses it - and twenty turns are one screenful
   * in one session and thirty in another. A screen and a half is enough to
   * keep the next ask one pull away without making the jump large.
   */
  const PAGE_VIEWPORTS = 1.5;
  /**
   * What the list reserves for a row it has not measured yet, in pixels.
   *
   * **A chosen seed, not virtua's own running median.** Left unset, the list
   * re-estimates from the rows it has measured and applies that median to
   * every unmeasured row, so a small row arriving below the reader was
   * reserved at the median: the measured 66.5px row was drawn in a 270px
   * slot (#1555). This is a short turn's height by choice rather than a
   * measurement - the measured arrival was smaller. A wrong seed is
   * corrected by the row's own measurement a frame later, and the follow's
   * overhang check holds the pin until it is.
   */
  const ROW_ESTIMATE_PX = 120;
  /** How long a row's arrival mark stays on it, past the animation. */
  const ARRIVE_MS = 300;
  /** How many turns of row keys are remembered before the dropped ones go. */
  const ARRIVED_TURNS = 64;
  /** The longest a return's hold waits for a paint before it lifts anyway. */
  const HOLD_MAX_MS = 250;
  /** How far above the last pin counts as the reader when no input preceded it. */
  const DISARM_SLACK = 48;
  /** How long after a wheel, touch or up-scrolling key its events read as the reader's. */
  const READER_WINDOW_MS = 250;
  /** The keys a focused row scrolls the column UP with, which are the ones that may disarm. */
  const SCROLL_UP_KEYS = new Set(['ArrowUp', 'PageUp', 'Home']);
  /** The keys that scroll the column DOWN, which aim the reader at the foot. */
  const SCROLL_DOWN_KEYS = new Set(['ArrowDown', 'PageDown', 'End']);

  /**
   * The conversation, held as a VALUE: a frame replaces the record rather than
   * changing it, so nothing in it needs tracking - deep state made every read
   * of every message a proxy call on each frame's re-derivation.
   */
  let held = $state.raw<Conversation>(NOTHING);

  // **The column reports its own `loaded` flag.** The band draws the read
  // wait from it, and this column is the only thing that knows when the first
  // page landed.
  $effect(() => {
    onloaded(held.loaded);
  });
  /**
   * The element that scrolls, which is the list's own viewport.
   *
   * Held so the follow can pin the foot by asking the BROWSER where it is:
   * the list's reported size is a measurement that lags a row that grew in
   * this update, and the element's own `scrollHeight` does not.
   */
  let viewport: HTMLElement | null = $state(null);
  /**
   * The viewport's own top padding, read once where it is bound.
   *
   * The list's measured size stops at its content, so this is the whole of
   * the gap between the browser's scroll numbers and the list's own - the
   * difference the foot pin has to account for when it holds at the model
   * instead of at the browser's end.
   */
  let padTop = 0;
  /**
   * The list's own handle, for the one move the element cannot make: a jump
   * to a row that may not be drawn. `land` deliberately goes through the
   * element; finding an arbitrary past row needs the list's index math.
   */
  let list: VListHandle | null = $state(null);
  /** Where the column last left the reader, which its own pin's echo cannot disarm. */
  let placed: number | null = null;
  /** The seat `placed` was recorded on, so a re-run for the same seat keeps it. */
  let placedFor: string | null = null;
  /** When the reader's own input was last seen; null until a hand touches the column. */
  let readerAt: number | null = null;
  /** When the reader's own input was last seen heading toward the foot; null until a hand aims there. */
  let aimedAt: number | null = null;
  /**
   * The row the reader's own eye is on, held while they are away from the foot.
   *
   * **Measured in rows rather than pixels** (the rule the view this client
   * replaced kept, and `anchor.ts` carries the reasoning): the layout moves
   * under a reader who is not at the foot - a row measures taller once drawn,
   * a stretch is corrected as frames land - and an offset that stays put reads
   * as the page sliding under them (Ved, 2026-10-03). It goes the moment the
   * follow is back on, because the foot is where that reader wants to be.
   */
  let anchor: Anchor | null = null;
  let working: Chat | null = null;
  /**
   * The conversation built for one seat over one connection.
   *
   * **It is keyed on the seat's own name rather than on the props**, because
   * the page re-derives the object it hands this column on every read it
   * makes, and a live seat re-reads on every frame it emits: an effect keyed on
   * the props is torn down and rebuilt under each of them, which leaves the
   * column empty for as long as the next list takes to measure. A key is a
   * string, and a string is written only when it changes.
   */
  const seat = $derived(subjectKey({ session: slot }));
  /**
   * The reader's words, from submit until the core's own copy of them lands.
   *
   * Held per seat by the client rather than by either column, because the
   * composer writes it and this one draws it: it is one mechanism over both
   * sending surfaces, and a seat the reader has left keeps its pending send.
   */
  const echo = $derived(echoes.of(seat));

  /** The newest turn, which is the one a running row is folded for. */
  const newestTurn = $derived(
    held.turns.length === 0 ? null : (held.turns[held.turns.length - 1] ?? null),
  );
  /**
   * The stops the page's turns carry, resolved once and handed to every turn's
   * fold: a job started in one prompt and stopped in the next has its stop in
   * a LATER turn than the call it ends, so no single turn can see both.
   */
  const stops = $derived(stopsOf(held.turns));
  /** The newest turn's key: the row the carried beat and the reader's echo ride. */
  const newest = $derived(newestTurn?.key ?? null);
  /**
   * The rows each turn has already shown, by their own `data-k` keys.
   *
   * **The row's key, not the DOM's shape.** The list re-creates a row's node
   * whenever it leaves and re-enters the drawn window, and a fold rebuild
   * re-creates it in place - so a mark read off the wrapper's last child
   * replays the entrance for rows the reader has already seen, and the same
   * row can enter several times inside one turn. The key is the one name a
   * row keeps across all of that, so a key seen once is never marked again.
   *
   * Seeded from what the column draws when it attaches, because those rows
   * arrived before anyone was watching for them.
   */
  const arrived: Record<string, string[]> = {};

  /** Whether this row has arrived in this turn before, recording it if not. */
  function firstArrival(turn: string, key: string): boolean {
    let rows = arrived[turn];
    if (rows === undefined) {
      rows = [];
      arrived[turn] = rows;
      // **A turn the window no longer holds cannot be drawn again**, so its
      // keys go once the record outgrows the turns a conversation can hold at
      // once. Pruned here rather than in an effect: a reactive write on the
      // arrival path schedules a render the entry's own pin is measured
      // against, and the list re-applies its attachment on a render.
      if (Object.keys(arrived).length > ARRIVED_TURNS) {
        const drawing = held.turns.map((turn) => turn.key);
        for (const seen of Object.keys(arrived)) {
          if (!drawing.includes(seen)) delete arrived[seen];
        }
      }
    }
    if (rows.includes(key)) return false;
    rows.push(key);
    return true;
  }

  /** Mark one row as having just arrived, and take the mark back after it. */
  function enter(row: HTMLElement): void {
    row.classList.add('arrive');
    // **The mark goes when the rise ends**, not when the fade does: the two
    // animations run at different lengths, and a class left on the node
    // would restart both if the row were moved in the document. The timer is
    // the fallback for a node the animation never reaches (a stilled one).
    const clear = (event: AnimationEvent): void => {
      if (event.animationName !== 'arrive-rise') return;
      row.classList.remove('arrive');
      row.removeEventListener('animationend', clear);
    };
    row.addEventListener('animationend', clear);
    setTimeout(() => {
      row.classList.remove('arrive');
      row.removeEventListener('animationend', clear);
    }, ARRIVE_MS);
  }

  /**
   * The rows one inserted node brought with it, outermost only.
   *
   * A new unit and the call row inside it arrive together, and entering both
   * would run the entrance inside itself; the outermost row is the one the
   * reader sees arrive. A row inserted inside a unit already drawn is its own
   * outermost, so a call joining a run enters on its own.
   */
  function rowsIn(node: Element, turn: string): HTMLElement[] {
    if (node.matches('[data-k]')) {
      return node.closest('[data-t]')?.getAttribute('data-t') === turn ? [node as HTMLElement] : [];
    }
    const out: HTMLElement[] = [];
    for (const row of node.querySelectorAll('[data-k]')) {
      if (row.closest('[data-t]')?.getAttribute('data-t') !== turn) continue;
      const outer = row.parentElement?.closest('[data-k]');
      if (outer !== null && outer !== undefined && node.contains(outer)) continue;
      out.push(row as HTMLElement);
    }
    return out;
  }

  /**
   * The echo goes the moment the conversation carries the words.
   *
   * **The core's own copy is the signal, in either carrier the wire uses**: a
   * prompt that starts a turn arrives as that message's own text, and one sent
   * while a turn is already running is held by the CLI as a queued block
   * instead. The composer's own backstop is the turn going in flight, which is
   * a signal a queued prompt never gives - so a reconcile that knew one
   * carrier would leave the row saying "sending" for the rest of the turn.
   *
   * **And it reads only the two ENDS of the newest turn.** The core's copy
   * lands at one end or the other - appended by the live path, at the head of
   * the turn a page read rebuilds - while a turn can be thousands of messages
   * long and this runs on every arriving frame. A scan of the whole thing is a
   * cost that grows with exactly the turn the reader is sending into, which is
   * #1591's shape one layer down.
   */
  $effect(() => {
    const held = echoes.of(seat);
    if (held === undefined) return;
    const turn = newestTurn;
    if (turn === null) return;
    if (!carries(turn.messages, held.words)) return;
    echoes.clear(seat);
  });

  /**
   * Whether a turn's own messages carry these words.
   *
   * **Each end, and stopped by the first frame that is not the reader's**: a run
   * of their own words is what either end of a turn holds - the live path
   * appends one, and a page read opens the turn with one - and everything
   * between is the work the turn did, which is where the length is.
   */
  function carries(messages: unknown[], words: string): boolean {
    const run = (from: number, step: number): boolean => {
      for (let at = from; at >= 0 && at < messages.length; at += step) {
        const message = messages[at];
        if (message === undefined) break;
        const said = ownWords(message);
        if (said.length === 0) break;
        if (said.includes(words)) return true;
      }
      return false;
    };
    return run(messages.length - 1, -1) || run(0, 1);
  }

  /**
   * Send the words again, from the row that says they did not go.
   *
   * The same dispatch the composer makes, because it is the same send: the row
   * hands back words the reader already typed rather than asking them to type
   * them again, which is what a refused send used to mean.
   */
  function retry(): void {
    const held = echoes.of(seat);
    if (held === undefined) return;
    // Whether this send starts a turn, read from the CONNECTION's own store
    // rather than from the turn this page draws - `runningAt`'s own doc has the
    // mechanism. A retry posted as not-running is taken by the very paint that
    // carries the turn, where a refusal can no longer reach it.
    //
    // The id is this send's own, freshly minted: the prompt the first attempt
    // sent is dead, so the retry is a new prompt that happens to say the same
    // words - and the pile settles each by id, never by text.
    const id = mintPromptId();
    // The press's own number, echoed by a refusal, exactly as the composer's
    // own send carries it (#1583): a reason arriving later names this send.
    const dispatch = echoes.mint();
    echoes.post(
      seat,
      held.words,
      runningAt(connection, slot, newestTurn?.running === true),
      id,
      dispatch,
    );
    try {
      // Fire-and-forget like the composer's own send: the outcome rides the
      // subscription rather than a reply, so there is nothing here to await.
      // No seat handed over: this retry's refusal is drawn by the echo row
      // below, with the retry button the reader came here for - a notice line
      // beside it would tell the same loss twice.
      void connection.dispatch({
        prompt_under: {
          key: slot,
          text: held.words,
          attachments: [],
          uuid: id,
          source: 'you',
          dispatch_id: dispatch,
        },
      });
    } catch {
      // A closed socket throws rather than answering, so the row says why
      // rather than the words going with a command that never left.
      echoes.refuse(seat, 'the socket is closed');
    }
  }

  /**
   * Ask the core for a call's own output, on the row's behalf.
   *
   * The ask rather than the read: the file the CLI streamed a backgrounded
   * command's output to is named by the task's own frames, which the core
   * holds - and the answer comes back as a `call_output` update this
   * conversation folds into the store the row reads.
   */
  function readOutput(callId: string): void {
    try {
      void connection.dispatch({ read_call_output: { key: slot, call_id: callId } }, slot);
    } catch {
      // A closed socket throws rather than answering: the row keeps the ack
      // it has, and the next open asks again.
    }
  }
  /**
   * Every seat's conversation, kept for the life of the connection.
   *
   * **Module state, not the column's** (`chat/conversations.ts`): the column
   * unmounts when the reader leaves the session page, and a Home round trip
   * must not lose what the frames kept current - the seat's subscription
   * outlives the page, so no snapshot comes back to seed a fresh one.
   *
   * **Nothing draws from either map, and every read of them is untracked**: the
   * seat on screen is `held` above, and a tracked read here would make the
   * effect below depend on the entry the subscription writes on every frame,
   * re-running it per frame with nothing about the seat changed.
   */
  const conversations = () => conversationsFor(connection);
  /**
   * Pages of older turns asked for and not yet answered.
   *
   * **It says a page is coming, never what the change will be** - which is why
   * the list is told the change itself (see `shift` below) rather than this
   * count. Counted rather than flagged because the asks are edges: an ask the
   * socket refused arms nothing, and a second ask still in flight must not be
   * disarmed by the first page landing.
   */
  let outstanding = $state(0);
  /**
   * Whether a seat the column already holds is being entered, until the frame
   * it drew has painted.
   *
   * **A return draws its held row before it pays a history read**: the paging
   * triggers ride the list's first measurements, which a browser delivers
   * before the paint, so the fetch could leave for a row the reader had not
   * seen (#2572 retro). The walk the hold refuses is run when it lifts.
   */
  let entering = false;
  /** Which entry the hold belongs to, so only the newest one lifts it. */
  let entered = 0;
  /** Whether the last prepend has landed but the layout has not settled yet. */
  let settling = $state(false);
  /** The prepend count this component has already accounted for. */
  let accounted = 0;
  /** The dropped-ask count this component has already drained against. */
  let drained = 0;
  /** The sizes at the last scroll event, which tell a reader moving from a layout moving. */
  let lastSeen = { content: 0, room: 0 };
  /** The offset the last pin wrote, so the event it comes back as is not read as the reader's. */
  let pinEcho: number | null = null;
  /** How many layout passes the observer has already put a parked reader back for. */
  let restored = 0;
  /**
   * The signature of the keys above the anchor that a parked pass has PAID
   * for: the restore has run against this order, so only a change to it owes
   * another pass.
   *
   * **Stamped when the pass runs, never when it is scheduled** (#1734's fix
   * round): the effect's teardown cancels a pending rAF on every re-run, and
   * a second publish landing before the paint is ordinary - so a stamp at
   * schedule time would drop the restore with the cancel, and the next run
   * would find the debt already paid and never re-arm. Held here, the cancel
   * leaves the debt standing.
   */
  let owed: string | null = null;
  /**
   * Whether the last restore could not place the reader at all.
   *
   * **A debt needs paying even when its signature is already stamped.** The
   * anchor row can be gone with no row holding the viewport's top edge - a
   * layout between states, or a capture run before the rows measured - and
   * `owed` alone would read that as "this order is paid", so no later publish
   * would try again. Cleared by any capture or restore that lands.
   */
  let owedUnplaced = false;
  /** The tick a settling compensation waits on, held so a later one can replace it. */
  let timer: ReturnType<typeof setTimeout> | null = null;

  /**
   * Whether the list is holding the reader by a prepend's own compensation,
   * which is what the column's passes stand aside for.
   */
  const compensating = $derived(outstanding > 0 || settling);

  /**
   * Whether the change that built this list moved its head, which is the only
   * change it may compensate for. The why is `movedHead` in `conversation.ts`;
   * this is only the read.
   */
  const shift = $derived(headMoved(held.turns));
  /** This column's own memory of its rows' heights, kept out of a module global. */
  const rowSizes = sizes();
  // The measured heights belong to the turns this conversation holds.
  $effect(() => rowSizes.prune(held.turns.map((turn) => turn.key)));

  /**
   * Whether a press on the scrollbar is still held down.
   *
   * The press arms the token for `READER_WINDOW_MS`, and a reader who presses
   * the thumb, aims for a second and only then drags would get nothing from
   * it - a drag fires no second pointerdown to re-arm (measured: a real drag
   * in Chromium and in WebKit delivers pointerdown and pointerup and NO moves
   * between them). So the press also sets this hold, the window's own
   * pointerup/pointercancel clears it - the pair the drag does deliver - and
   * while it is set the hand counts whatever the clock says.
   */
  let readerHeld = false;

  /** Whether the events arriving are the reader's own, made moments ago. */
  function readerMoved(): boolean {
    if (readerHeld) return true;
    const seen = readerAt;
    return seen !== null && performance.now() - seen < READER_WINDOW_MS;
  }

  /** Whether the reader's own input, moments ago, was heading toward the foot. */
  function aimedAtFoot(): boolean {
    const seen = aimedAt;
    return seen !== null && performance.now() - seen < READER_WINDOW_MS;
  }

  /**
   * The list's viewport, taken as an attachment.
   *
   * The list spreads what it does not recognise onto the element that
   * scrolls, so this is that element rather than a second wrapper: `.conv` is
   * the thing the sheet gives its padding and its scrollbar to, and it is the
   * one whose `scrollHeight` is the foot the reader is pinned to.
   *
   * **And it is watched, with its content**, because a size change is the one
   * thing that moves the foot with no frame and no scroll behind it: the rows
   * are laid out and re-measured after this column's own effects have run, so
   * the first pin of a page can land before there is anything to scroll, and a
   * row that grows minutes later moves the foot again with nothing to re-pin.
   *
   * The same event is the other re-arm the view this client replaced had: its
   * clamp re-engaged the follow whenever the content fit under the reader - a
   * window made large enough, a page that shrank - and a reader parked at what
   * is now the very end is at the end, whether or not anything scrolled to say
   * so.
   */
  /**
   * Record what a turn's row measures, so a row the send's pull-and-return
   * keeps mounted holds its own space while it draws nothing (#1890).
   */
  function measureTurn(node: HTMLElement, key: string): () => void {
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) rowSizes.remember(key, entry.contentRect.height);
      // **The row's own report wakes the fill.** A landing's rows measure
      // AFTER the size watcher's batch has already read them as unmeasured,
      // and a row that measures at the list's estimate leaves the content
      // height where it was - so no second watcher batch comes, and without
      // this nothing asks the page above (#2411).
      fillFittingRoom();
    });
    observer.observe(node);
    return () => observer.disconnect();
  }

  function scrollViewport(node: HTMLElement): () => void {
    viewport = node;
    padTop = Number.parseFloat(getComputedStyle(node).paddingTop) || 0;
    // A token armed against a previous viewport must not eat this one's first event.
    pinEcho = null;
    // The sizes and the last event belong to the element a previous conversation scrolled.
    lastSeen = { content: 0, room: 0 };
    movedFrom = 0;
    // The reader's own input, and the only evidence of a hand the column has:
    // scroll events carry no source, so the gesture has to be heard separately.
    // The UP-capable kinds are the disarm's - a gesture that cannot move the
    // reader up cannot have moved them up (the view this client replaced
    // disarmed on an up-scroll alone). The DOWN-capable kinds are the arm's:
    // a reader heading for the foot lands there through a clamp whose offset
    // never rose. A touch and a press on the scrollbar arm whole: neither
    // carries a direction.
    const arm = () => {
      readerAt = performance.now();
    };
    const aim = () => {
      aimedAt = performance.now();
    };
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) arm();
      else if (event.deltaY > 0) aim();
    };
    const onKey = (event: KeyboardEvent) => {
      if (SCROLL_UP_KEYS.has(event.key) || (event.key === ' ' && event.shiftKey)) arm();
      else if (SCROLL_DOWN_KEYS.has(event.key) || event.key === ' ') aim();
    };
    /**
     * **The press that starts a scrollbar drag, which has no wheel to hear.**
     * A drag fires neither a wheel nor a touch, so its moves were read only
     * past `DISARM_SLACK` above the pin's landing - and while the follow is
     * on, the pin re-writes the foot on every arriving update, so a drag had
     * to out-run the pin before the disarm could land (Ved, 2026-10-16: "I
     * have to make an effort to actually move it up, and it is still fighting
     * me"). The press lands on the pane's own box - a scrollbar is not a row,
     * and no row covers the gutter - so the hand is heard where the drag
     * begins, and the disarm takes at any distance like a wheel's.
     *
     * **The offset is the second clause because the target alone is not
     * enough.** While `virtua` is scrolling it sets `pointer-events: none` on
     * the content container for 150ms after the last scroll, so a press on a
     * ROW then falls through and fires with the pane as its target - which
     * would arm the token for a click on the conversation. Measured with real
     * input on the live seat: a scrollbar press reads an offset past
     * `clientWidth` (854 against 850 in Chromium, 934 against 930 in WebKit),
     * the pane's own padding strip reads under it (846, 926), and the
     * fall-through press reads under it too (200, and 199 in WebKit, where
     * the press still found the row).
     */
    const onPress = (event: PointerEvent) => {
      if (event.target === node && event.offsetX >= node.clientWidth) {
        arm();
        readerHeld = true;
      }
    };
    /**
     * The release, which the drag does deliver, gives the hand back.
     *
     * **And so does losing the window**, because a release outside the app
     * never arrives: the pointerup belongs to whatever took the pointer, and
     * without a fallback the hold would stand for the life of the seat - which
     * is the one state this flag can get stuck in and is invisible on screen.
     * The old 250ms token self-healed; this cannot, so every exit clears it.
     */
    const onRelease = () => {
      readerHeld = false;
    };
    node.addEventListener('wheel', onWheel, { passive: true });
    node.addEventListener('touchstart', arm, { passive: true });
    node.addEventListener('touchmove', arm, { passive: true });
    node.addEventListener('keydown', onKey);
    node.addEventListener('pointerdown', onPress);
    // Window-level, because the drag's pointerup lands on the window when the
    // pointer has left the pane - and a hold with no release would count every
    // later nudge as the reader's.
    window.addEventListener('pointerup', onRelease);
    window.addEventListener('pointercancel', onRelease);
    // The release that never arrives, for the press let go outside the app.
    window.addEventListener('blur', onRelease);
    const content = node.firstElementChild;
    const watcher = new ResizeObserver(() => {
      if (holdsEverything()) {
        working?.following(true);
        anchor = null;
      }
      // The layout has settled, so the reach ask may have no scroll to ride
      // (#2139). **The fit itself is read inside `fillFittingRoom` and not
      // here**: the empty page's beat reaches that function on its own timer,
      // without passing this wrapper, so a guard only here would let a column
      // that overflowed during the beat ask a page it does not need.
      fillFittingRoom();
      if (held.following) land();
      // A reader away from the foot has a place of their own, and a size change
      // is one of the two ways it moves under them. **Not while the prepend
      // compensation is on**, same as the effect's own pass: that path holds
      // the reader by the list's own compensation as older turns arrive above
      // them, and two hands on the scroll is one too many.
      else if (!compensating) {
        restored += 1;
        restoreAnchor();
      }
    });
    watcher.observe(node);
    if (content !== null) watcher.observe(content);
    // **Rows enter here, once per key.** The observer is on the scroller
    // rather than on the newest turn's wrapper because the wrapper itself is
    // re-created with the list's window; a key already in `arrived` is a row
    // the reader has seen, whoever draws it again.
    const arrivals = new MutationObserver((records) => {
      // **Only a turn the frames built is a live turn.** A page read is
      // history rather than arrival, so the rows a load draws are drawn
      // still; from the frame that opens a turn onward, every row it takes
      // enters once.
      const turn = untrack(() => newestTurn);
      if (turn === null || !turn.live) return;
      const at = turn.key;
      for (const record of records) {
        for (const added of record.addedNodes) {
          if (!(added instanceof Element)) continue;
          for (const row of rowsIn(added, at)) {
            const key = row.getAttribute('data-k');
            if (key === null || !firstArrival(at, key)) continue;
            enter(row);
          }
        }
      }
    });
    // What is already drawn has already arrived: a scroll back to it, or a
    // fold rebuild, must draw it without replaying the entrance.
    for (const row of node.querySelectorAll('[data-k]')) {
      const turn = row.closest('[data-t]')?.getAttribute('data-t');
      const key = row.getAttribute('data-k');
      if (turn !== null && turn !== undefined && key !== null) firstArrival(turn, key);
    }
    arrivals.observe(node, { childList: true, subtree: true });
    return () => {
      arrivals.disconnect();
      watcher.disconnect();
      node.removeEventListener('wheel', onWheel);
      node.removeEventListener('touchstart', arm);
      node.removeEventListener('touchmove', arm);
      node.removeEventListener('keydown', onKey);
      node.removeEventListener('pointerdown', onPress);
      window.removeEventListener('pointerup', onRelease);
      window.removeEventListener('pointercancel', onRelease);
      window.removeEventListener('blur', onRelease);
      // **A hold does not outlive its attachment.** The list unmounts mid-drag
      // - an empty or unloaded conversation draws no list - and the release
      // that would have cleared the flag lands after the listener is gone.
      readerHeld = false;
      viewport = null;
    };
  }

  /** The drawn rows' boxes, measured one at a time as the scan asks for them. */
  function* drawnRows(): Generator<RowBox> {
    if (viewport === null) return;
    for (const row of viewport.querySelectorAll('[data-k]')) {
      const box = row.getBoundingClientRect();
      yield { key: row.getAttribute('data-k') ?? '', top: box.top, bottom: box.bottom };
    }
  }

  /**
   * Hold the row the reader's top edge is on, which is what their place means,
   * answering whether a row could be held.
   *
   * **No row at the top edge is not a place** - the column can be between
   * layouts, or the reader below everything drawn - so a capture that finds
   * none leaves the old anchor standing and says so, which is what lets the
   * pass retry.
   *
   * **The debt is the caller's.** A capture from the reader's own move
   * settles it where it is taken (`owed = null` at those call sites); a
   * capture inside a restore pass is followed by that pass's own stamp, so
   * clearing it here would be overwritten and would lie to the reader of
   * this function.
   */
  function captureAnchor(): boolean {
    if (viewport === null) {
      owedUnplaced = true;
      return false;
    }
    const landed = anchorAt(drawnRows(), viewport.getBoundingClientRect().top);
    if (landed === null) {
      owedUnplaced = true;
      return false;
    }
    anchor = landed;
    owedUnplaced = false;
    return true;
  }

  /**
   * Put the reader back on the row the anchor holds, wherever the layout moved
   * it to, answering whether the pass placed them somewhere.
   *
   * **A lookup that misses re-captures from the viewport** (#1834): the fold
   * can re-key a row in place, or take it away, and the reader's place is
   * still whichever row holds the viewport's top edge - which is also true of
   * a row that is merely outside the drawn window. A capture that finds no
   * row either is the one case a pass cannot pay, and its caller keeps the
   * debt so the next publish tries again.
   *
   * **The key is the turn's own plus the unit's**, which `Turn` writes, so the
   * lookup cannot land on a row of another turn: the fold names an id-less
   * frame `f<index>` within its own turn, and two turns can each carry one.
   */
  function restoreAnchor(): boolean {
    const held = anchor;
    if (held === null || viewport === null) return true;
    const row = viewport.querySelector(`[data-k="${CSS.escape(held.key)}"]`);
    if (row === null) return captureAnchor();
    const box = viewport.getBoundingClientRect();
    const top = row.getBoundingClientRect().top - box.top + viewport.scrollTop;
    const want = anchoredScroll(held, top);
    if (Math.abs(want - viewport.scrollTop) >= 1) viewport.scrollTop = want;
    owedUnplaced = false;
    return true;
  }

  /**
   * Whether the reader sits at the very end of what the list holds.
   *
   * **The element's own clamp, not the list's reported size**: that is a model
   * - estimates for rows never drawn, a viewport unmeasured in the flush a pin
   * runs in - under which a reader the browser has clamped to the foot still
   * reads as short of it.
   */
  function atFoot(): boolean {
    return viewport !== null && viewport.scrollTop + viewport.clientHeight >= viewport.scrollHeight;
  }

  /**
   * Whether the whole conversation fits, which is the only size change that
   * re-arms the follow.
   *
   * **A size change is not the reader moving**, and the column's height is not
   * only the conversation's: the composer below it changes shape - a dictation
   * row appearing, its panel closing - and a reader a little way up measures as
   * being at the very end once the column has taken the room, because they are
   * at the end of what now fits. That arithmetic is right and the conclusion is
   * wrong: the end is not where they put themselves. What is left is the case
   * this arm exists for, where nothing is left to scroll and being at the end
   * is not a position anyone chose.
   */
  function holdsEverything(): boolean {
    return viewport !== null && viewport.scrollHeight <= viewport.clientHeight;
  }

  /**
   * Settle the held window on the newest page where the reader has reached the
   * foot or a page has landed under them.
   *
   * **The trim is bounded by the row at the viewport's top edge**, so nothing
   * the reader can see goes: a fitting column's top row is row 0, which makes
   * the trim a no-op there without a second guard.
   */
  function settleWindow(): void {
    // **The budget is decided from the record, before any layout read**: a
    // frame lands many times a second and a scroll height read per frame is
    // the cost #1710 removed. Only a held set over the window pays one.
    if (held.bytes <= HISTORY_WINDOW_BYTES) return;
    const at = viewport === null || list === null ? null : list.findItemIndex(viewport.scrollTop);
    working?.trim(at === null ? null : (held.turns[at]?.key ?? null));
  }

  $effect(() => {
    const which = seat;
    const open = connection;
    const { kept, live } = conversations();
    let entry = untrack(() => live.get(which));
    /** Whether this run carries a conversation the column did not already hold. */
    let fresh = false;
    // A seat reopened on another connection is a different conversation, so the
    // one held goes with the socket that carried it.
    if (entry !== undefined && entry.connection !== open) {
      entry.stop();
      live.delete(which);
      kept.delete(which);
      entry = undefined;
    }
    if (entry === undefined) {
      const chat = new Chat(open, slot);
      const stop = chat.start();
      entry = {
        connection: open,
        chat,
        stop,
      };
      live.set(which, entry);
      fresh = true;
    }
    working = entry.chat;
    // **Every seat folds; only the shown one draws.** The conversation folds
    // every frame it receives wherever the seat is (the seat's own
    // subscription is the client's and outlives the page - Ved, 2026-10-25),
    // so the state never falls behind;
    // whether the seat is on screen decides only which value this column
    // renders. Written to the seat's own entry rather than straight to
    // `held`: a conversation kept for a seat the reader has left must not
    // draw.
    entry.chat.showing();
    const unsubscribe = entry.chat.value.subscribe((value) => {
      kept.set(which, value);
      if (untrack(() => seat) === which) held = value;
    });
    // The placement belonged to the conversation that is going, and the two
    // clauses are the two ways one goes. `fresh` is a conversation this run
    // opened: a seat's first visit, or a seat re-opened on another connection.
    // The seat compare is a kept conversation - one this column still holds
    // from an earlier visit - coming back on screen with a placement measured
    // on the seat the reader just left, which left standing would be another
    // seat's number read as this view's own.
    //
    // **A re-run that changes neither is not a conversation change**, and the
    // placement is a fact about the reader's element rather than about the
    // read - so clearing it on every re-run is what let the landing's own pin
    // read as an un-pinned column and switch the follow off mid-open (measured
    // on the switch into the giant seat: the reader left 175,859px above the
    // foot).
    //
    // **A third way one goes is deliberately not named here**: an occupant swap
    // on the same seat and connection (`session_replaced`) keeps the placement,
    // and that is benign only because the swap empties the conversation, so the
    // list unmounts and the new landing re-pins before any event can read the
    // old value. A swap that kept the list mounted across it would make this
    // stale.
    // **The same predicate is what a conversation OPEN is** (#2374): a first
    // visit, a seat reopened on another connection, or a kept seat coming
    // back on screen. Every way into a seat - a click, a deep link, a reload,
    // a reconnect, a palette move - passes through this effect, so the perf
    // log's open is marked from here rather than from a route change: a route
    // change alone is no conversation, and a board route must write nothing.
    const switched = fresh || which !== placedFor;
    if (switched) {
      placed = null;
      // The place a reader held was a row of the conversation that is going.
      anchor = null;
      // And so were the sizes and the last event, which read that element.
      lastSeen = { content: 0, room: 0 };
      movedFrom = 0;
      // **And so is the follow, which is why the entry re-arms it** (#1673):
      // a seat's conversation is kept, and its follow flag was kept with it -
      // so a seat left scrolled up came back with the pass returning early
      // and the reader landing wherever the old offset fell. A place belongs
      // to the visit; every entry lands at the latest. The window is settled
      // by the follow effect below, which runs on the arriving conversation's
      // own publish - this call would hand the outgoing seat's row key to the
      // incoming chat's trim.
      working?.following(true);
      // **And so were the asks this column is holding.** They belong to the
      // conversation that is going, and their pages are not coming here.
      outstanding = 0;
      settling = false;
      // **And the hand is not this seat's.** A hold that outlived its press
      // belongs to the gesture that left; the one state this flag can get
      // stuck in is invisible on screen, and the time-token it replaced
      // self-healed in 250ms where this does not.
      readerHeld = false;
      aimedAt = null;
      // **A return holds the walk until its own draw has painted.** Two
      // frames, because a frame's callbacks run before its paint. A first
      // visit has no held row to protect, and a hidden page has no paint.
      const mine = (entered += 1);
      entering = !fresh && !cannotPaint();
      if (entering) {
        let deadline: ReturnType<typeof setTimeout> | null = null;
        const lift = (): void => {
          if (entered !== mine || !entering) return;
          entering = false;
          if (deadline !== null) clearTimeout(deadline);
          deadline = null;
          // **The lift pays both automatic triggers the hold refused.** The
          // fit, and the pin echo's reach ask - that band is serviced by no
          // other ask, so the offset is read again here and the page still
          // owed. Whichever goes first, the other finds the ask in flight.
          if (viewport !== null && viewport.scrollTop < REACH) loadOlder(pageTurns());
          fillFittingRoom();
        };
        // **A throttled window produces no frames**, so the hold carries its
        // own deadline and cannot leave the fit waiting on a paint that is
        // not coming.
        deadline = setTimeout(lift, HOLD_MAX_MS);
        requestAnimationFrame(() => requestAnimationFrame(lift));
      }
    }
    placedFor = which;
    // The seat coming on screen is put there from what was kept, not from a
    // read, which is the whole point of holding it.
    held = untrack(() => kept.get(which)) ?? NOTHING;
    // After `held`, so the open's own start and the conversation it names
    // land in one flush - the drawn mark below reads `held` and must never
    // time the seat that is going. The seat is handed over as the slot and
    // named by the log's own builder, so this column and a frame's surface
    // pair can never spell one seat two ways.
    if (switched) {
      noteConversationOpened(slot);
    }
    // Leaving the seat: its store notifications stop with its frames.
    return () => {
      unsubscribe();
    };
  });

  // A hold that outlived its column must not ask for a seat nothing draws.
  $effect(() => () => {
    entered += 1;
  });

  /**
   * The seat the column last showed, so a real switch can hand the one it
   * left back.
   *
   * **Not read from the effect's cleanup**: cleanup runs before the next run
   * and cannot see whether the seat changed, while a re-run for the same seat
   * (the page handing props over again) is not a leave - leaving there would
   * answer with a refresh the handover never asked for.
   */
  let shownFor: string | null = null;
  $effect(() => {
    const which = seat;
    const previous = shownFor;
    shownFor = which;
    if (previous !== null && previous !== which) {
      untrack(() => conversations().live.get(previous))?.chat.leaving();
    }
  });

  /**
   * **The seat's terminal drawing (#2374).** The effect runs after the
   * update that first puts this seat's answer on screen, which is what the
   * switch's own timing is measured to - the mark is written where the
   * drawing landed, not where the click happened. Declared after the seat
   * effect above, so a switch has put the new seat's conversation in `held`
   * before this reads it: a mark written off the seat that is going would
   * time the wrong open.
   *
   * **Every terminal state draws a mark, and the mark says which one.** The
   * chain below mirrors the column's own state chain: the waking state (a
   * seat the roster does not name, or a spawn in flight - the composer's band
   * now draws its line, and the not-running line is the same state and shares
   * the word), then the refusal, then the read wait (the band's too), then the
   * empty state, then the rows. A waking or refused line is NOT the seat's content: the probe
   * records it as a placeholder and keeps the open armed, so the content's
   * later drawing is the one that measures click-to-first-row, and only it
   * lets a quiet follow.
   */
  function terminalState(): Parameters<typeof noteDrawn>[0] | null {
    if (held.turns.length === 0 && (waking || spawning)) return 'waking';
    if (held.turns.length === 0 && held.refused !== null) return 'refused';
    if (!held.loaded) return null;
    return held.turns.length === 0 ? 'empty' : 'conversation';
  }

  /**
   * What the column measured where the drawn mark is written.
   *
   * **A drawn mark is a state, not a pixel**: this reads `held`, so a frame
   * where the record holds rows and the column drew none scores as drawn. The
   * geometry is what tells the two apart - a black column is one whose model
   * holds turns and whose viewport has no row with a box, or whose pin refused
   * itself on an undrawn newest row. Written once per open, never per frame.
   */
  function drawnGeometry(): DrawnGeometry {
    // **No list is a reading, not an absence.** A conversation mark with no
    // geometry at all cannot be told from one whose measure was skipped, and
    // `rows: 0` says the column drew no list to measure.
    if (viewport === null) {
      return {
        scroll_top: 0,
        scroll_height: 0,
        client_height: 0,
        rows: 0,
        rows_visible: 0,
        pin_refused: false,
      };
    }
    const rows = viewport.querySelectorAll('[data-t]');
    let visible = 0;
    for (const row of rows) {
      if (row instanceof HTMLElement && row.offsetHeight > 0) visible += 1;
    }
    return {
      scroll_top: viewport.scrollTop,
      scroll_height: viewport.scrollHeight,
      client_height: viewport.clientHeight,
      rows: rows.length,
      rows_visible: visible,
      pin_refused: undrawnOverhang() > 0,
    };
  }

  $effect(() => {
    const state = terminalState();
    // **Only the rows' mark has a list to measure.** A waking or refused line
    // draws no list, and the empty state draws no turns. The measure is handed
    // over as a thunk: this effect runs on every update the column holds, and
    // the probe writes the mark once per open.
    if (state !== null) {
      noteDrawn(state, state === 'conversation' ? drawnGeometry : undefined);
    }
  });

  // The first page is drawn at the end rather than the start, and that is not
  // a special case: the column OPENS following, so the first page is pinned by
  // the follow below like any other. A reader who scrolls away from the end
  // turns the follow off and the column stays where they left it.

  // A page landing settles one ask, and the last one leaves the compensation on
  // for one more tick: the landing's own layout pass is the one the column's
  // restore must stand out of the way of.
  $effect(() => {
    const now = held.prepends;
    if (now === accounted) return;
    accounted = now;
    outstanding = Math.max(0, outstanding - 1);
    settling = true;

    // Held in a variable rather than returned as this effect's cleanup: the
    // effect re-runs on EVERY update - `held` is a store read, so each one
    // hands over a new object - and a returned cleanup is run before each
    // re-run, which would cancel this timer on the next frame that arrived.
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      settling = false;
      timer = null;
    }, 0);
  });

  // **A forgotten ask drains the same count**, and no page is coming to do it:
  // a dropped socket takes the page in flight with it and a refusal answers
  // with none. Without this the count outlives the ask it was armed for.
  $effect(() => {
    const now = held.dropped;
    if (now === drained) return;
    drained = now;
    outstanding = 0;
    accounted = held.prepends;
    settling = false;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  });

  // The timer goes with the column, which is the one thing the effect above
  // cannot do for itself.
  $effect(() => () => {
    if (timer !== null) clearTimeout(timer);
  });

  /**
   * The newest turn's own report row - the unit, and the figures in it -
   * folded the way the turn folds itself.
   *
   * **One fold and one pick, because the row has two readers**: the pin draws
   * it, and the turn's own copy of it stands aside. Which row that is is
   * answered here rather than worked out again at each of them - and the fold
   * itself is shared with the turn's own, so one arriving frame is folded
   * once rather than once per reader.
   */
  const newestRow = $derived.by((): { key: string; info: TurnInfo } | null => {
    const turn = newestTurn;
    if (turn === null) return null;
    const units = turnFold(turn, slot, beingWritten(turn), !beingWritten(turn), stops);
    for (let at = units.length - 1; at >= 0; at -= 1) {
      const unit = units[at];
      if (unit !== undefined && unit.kind === 'report') return { key: unit.key, info: unit.info };
    }
    return null;
  });

  /**
   * How long the finished row holds before it detaches into the turn.
   *
   * **A twin of the composer's beat rather than one shared value**: that one
   * holds a landed take's border green (`Composer.svelte`), this one holds a
   * finished turn's row, and the two are the same idiom at the same length.
   */
  const BEAT_MS = 450;

  /** The turn whose row the pin is holding, which is what the beat is about. */
  let carried: string | null = $state(null);
  /** Whether the row being held is the finished one, on its beat before it detaches. */
  let beating = $state(false);
  /** The beat's own timer, held rather than returned as a cleanup: a frame arriving mid-beat re-runs this effect. */
  let beatTimer: ReturnType<typeof setTimeout> | null = null;

  function stopBeat(): void {
    if (beatTimer !== null) {
      clearTimeout(beatTimer);
      beatTimer = null;
    }
  }

  // The pin follows the newest turn: a turn being written takes the row, and a
  // turn that finished under the pin keeps it for one beat before it detaches
  // into the turn.
  //
  // **The effect re-runs on every frame**, so what it reads of its own state it
  // reads untracked - a tracked read of `carried` would make its own write a
  // reason to run again, and the beat a reason to re-arm itself forever.
  $effect(() => {
    const which = newest;
    const holding = newestRow !== null && newestRow.info.running;
    if (which === null || newestRow === null) return;
    if (holding) {
      // A turn being written takes the row back, whichever turn was beating
      // under it - the next turn can start inside the last one's beat.
      if (untrack(() => carried) !== which || untrack(() => beating)) {
        carried = which;
        beating = false;
        stopBeat();
      }
      return;
    }
    // A row that arrived finished is the turn's own, not the pin's: only the
    // turn this column watched run is carried over, and only once.
    if (untrack(() => carried) !== which || untrack(() => beating)) return;
    carried = which;
    beating = true;
    stopBeat();
    beatTimer = setTimeout(() => {
      beating = false;
      carried = null;
      beatTimer = null;
    }, BEAT_MS);
  });

  // The timer goes with the column, which is the one thing the effect above
  // cannot do for itself.
  $effect(() => () => stopBeat());

  /**
   * The row the pin is holding right now, if any: the whole of the turn being
   * written, and the beat after it ends.
   *
   * **One fact with two readers**, and it is the ROW rather than a yes: the
   * pin draws it, and the turn's own copy of that same row stands aside. The
   * key is checked rather than trusted, so a beat that outlived its turn - an
   * occupant swapped under the page - holds nothing.
   */
  const pinned = $derived(
    newestRow !== null && (newestRow.info.running || (beating && carried === newest))
      ? newestRow
      : null,
  );

  /**
   * What the follow pass runs on: the seat, the turn count, and every HEIGHT
   * the end of the conversation moves with.
   *
   * **The seat travels in the key as consistency, not as the mechanism**:
   * what re-runs the pass on a switch is the arriving conversation's own
   * record being published to the column, which the effect watches (measured:
   * six constructions tried, none where the key decides) - so the seat is in
   * the key so two seats with the same number of turns cannot collide, belt
   * and braces beside the publish that does the work (#1673).
   *
   * **The height term is #1759's other half.** A row measures after the frame
   * that grew it has published, and the beat's hand-over moves the newest row
   * with no publish at all - so the pass runs on the newest row's own measured
   * height, which is an observer of a different node from the column's content
   * and survives the notification a browser drops under load.
   */
  const follows = $derived(
    held.turns.length === 0
      ? null
      : `${seat}:${held.turns.length}:${rowSizes.sizeOf(newest) ?? ''}`,
  );

  /**
   * The beat's hand-over is a height no frame carries: effects run after the
   * DOM update, so this lands on the layout the moved row made.
   */
  let wasBeating = false;
  $effect(() => {
    const now = beating;
    if (wasBeating && !now && held.following) land();
    wasBeating = now;
  });

  // The composer's surface draws this row now, so the column publishes it: the
  // strip lane lives in the foot, and the fold that mints the row lives here.
  // Two effects because the cleanup must fire on a seat CHANGE and on unmount,
  // not on every frame that republishes the row.
  $effect(() => {
    const key = seat;
    return () => pinnedRow.set(key, null);
  });
  $effect(() => {
    pinnedRow.set(seat, pinned?.info ?? null);
  });

  /**
   * The dispatches the loaded conversation can reach, published for the
   * agents list.
   *
   * **A list entry that cannot lead to its row must not be shown.** The
   * record's list is the session's whole history while these turns are a
   * window of it, so an instance whose dispatch is not in a loaded turn -
   * and whose turn the transport can no longer page back to - would give a
   * click that goes nowhere.
   */
  $effect(() => {
    subagents.syncReachable(reachableIds(held.turns));
  });

  /**
   * How much of the foot is a slot the list has reserved for the newest turn
   * and not drawn yet.
   *
   * **Both sides in the list's own coordinates**, so the scroll element's own
   * padding stays out of it: the container's committed height minus the
   * item's own box, read live. Zero where nothing can be read - the newest
   * turn outside the drawn window, a DOM with no layout - which is the honest
   * answer where a reservation and a drawn row cannot be told apart.
   */
  function undrawnOverhang(): number {
    if (viewport === null || newest === null) return 0;
    const wrapper = viewport.querySelector(`[data-t="${CSS.escape(newest)}"]`);
    const item = wrapper?.parentElement;
    const container = item?.parentElement;
    if (!(item instanceof HTMLElement) || !(container instanceof HTMLElement)) return 0;
    const drawn = item.offsetHeight;
    const reserved = container.offsetHeight - item.offsetTop;
    return drawn < reserved - 1 ? reserved - drawn : 0;
  }

  /**
   * Pin the foot: the scroll's own maximum, which is where the browser clamps.
   *
   * **Through the element rather than through the list's handle, and that is
   * what makes it land.** `scrollToIndex` computes its target from the row
   * sizes the list has already measured, and a row that grew in this very
   * update is measured a moment LATER - so a scroll computed that way lands at
   * the end of the previous total and leaves the newest content below the fold
   * until the next frame happens to arrive, which on a turn that has just
   * settled is never. Asking the element for `scrollHeight` cannot be early or
   * late for a row the list has DRAWN: the browser clamps it to that row's own
   * foot - and where the number counts a row not drawn yet, the reservation
   * check below holds the pass back rather than land on it.
   *
   * The view this client replaced pinned the same way: while `auto_scroll`
   * was on, every render set its scroll target to its own max.
   */
  function land(): number {
    if (viewport === null) return 0;
    const foot = viewport.scrollHeight;
    // **Except where that number counts a row the list has not drawn yet.**
    // An appended turn is reserved a slot at the list's estimate and drawn a
    // frame later, so the foot runs past the content: pinning to it lands the
    // reader on empty space, and the clamp back - the row measures shorter
    // than the estimate - sweeps the window down and up again, dropping and
    // re-creating every row in the band. That is the blink on a send, where a
    // turn is appended while the reader is at the foot and the sweep crosses
    // the rows they are looking at. The pass waits for the draw instead; the
    // size change the measurement lands re-fires it.
    //
    // **Only an overhang counts, and only the newest turn's.** A foot SHORT
    // of the drawn rows is the model lagging a row that grew, where the pin
    // is what pulls the reader down after it; and a reader away from the foot
    // has the newest turn outside the window, where their pin is the jump
    // down to it and must go through.
    if (undrawnOverhang() > 0) return foot;
    // **Hold at the list's own bottom while the browser's runs ahead of it.**
    // A row that grew in this update is already in the browser's scrollHeight,
    // and the list measures it a callback later. A scroll written past the
    // measured content makes the list compute its window past its own end, so
    // it drops rows that are on screen and draws them again when the
    // measurement lands - the whole conversation flickering while a turn is
    // live. The pin stops at the model and lands again next frame, when the
    // growth is measured.
    const max = foot - viewport.clientHeight;
    const model = modelFoot();
    let target = foot;
    if (model !== null && max > model) {
      // **The pin never passes the end the list has measured.** It holds
      // there and comes back for the growth on a later frame; the two-frame
      // budget bounds only the deferred retries, never the hold, so a stream
      // that grows the row every frame still cannot write past the model.
      target = model;
      if (repinTries < REPIN_TRIES) {
        repinTries += 1;
        repin();
      }
    } else {
      repinTries = 0;
    }
    // **The write is unconditional, and the token comes from the read-back.**
    // `scrollTop` clamps to `scrollHeight - clientHeight`, so what the write
    // lands on is not what it asked for, and only the read-back knows whether
    // the offset moved - a write of the value the element already holds is
    // not a scroll and fires no event, so arming the token for it would leave
    // it armed with no echo coming, to be eaten by the next real event the
    // browser fires - a clamp the follow then misreads as the reader.
    const was = viewport.scrollTop;
    viewport.scrollTop = target;
    if (viewport.scrollTop !== was) pinEcho = viewport.scrollTop;
    // Where the column last left the reader, which is the foot the browser
    // clamped the pin to.
    placed = viewport.scrollTop;
    return foot;
  }

  /** How many deferred retries one hold at the list's model schedules. */
  const REPIN_TRIES = 2;
  /** How many of those frames the current hold has already spent. */
  let repinTries = 0;
  /** A pin deferred to the next frame, while the list measures the row that grew. */
  let repinFrame: number | null = null;

  /**
   * Land again once the list has measured what the browser already counted.
   *
   * **A frame, not a tick**: the list's measurement is a ResizeObserver
   * callback, delivered before the next frame's callbacks - so the next
   * animation frame is the first moment the pin can be written against it.
   * One frame is held at a time, and a reader who has left the foot by then
   * is left where they are.
   */
  function repin(): void {
    if (repinFrame !== null) return;
    repinFrame = requestAnimationFrame(() => {
      repinFrame = null;
      if (held.following) land();
    });
  }

  /**
   * The bottom the list itself has measured, as a scroll offset; `null`
   * before the list has a handle to ask.
   *
   * **The element's own room, not the list's report of it.** What can lag
   * behind the browser is the content the list has measured, and a column
   * whose room changed - the composer taking space - must pin on that change
   * at once rather than wait for a measurement nothing moved.
   *
   * `padTop` is exact while the column carries no bottom padding: the
   * browser's scrollable end counts the pane's top padding and the list's
   * measured content stops below it. A bottom padding would sit on the
   * browser's end alone and need a term of its own.
   */
  function modelFoot(): number | null {
    if (list === null || viewport === null) return null;
    return Math.max(0, list.getScrollSize() - viewport.clientHeight) + padTop;
  }

  /** The ask already answered, so a repeat of the same token is not acted on twice. */
  let answeredAsk: number | null = null;
  /** An ask still being worked: its row is not loaded yet, and pages are being pulled. */
  let asking = false;

  /**
   * The header's ask: reveal the latest compaction.
   *
   * Through the list's handle rather than the element, because the row may
   * not be drawn - and the jump it performs lands past the notch, so the
   * follow turns off the way it does when anyone scrolls away from the foot.
   * Nothing else has to remember it.
   *
   * **The cut is often older than what is loaded**, so the ask walks the
   * history: each page that lands re-runs this effect, and it stops asking
   * when the history runs out rather than retrying forever.
   */
  $effect(() => {
    const ask = $scrollAsk;
    if (ask !== null && ask.token !== answeredAsk) {
      answeredAsk = ask.token;
      asking = true;
    }
    if (!asking || !held.loaded) return;
    // A dispatch's row may sit in a turn the virtualised list has not drawn:
    // scroll to its turn first, then chase the row, which mounts a frame
    // after the scroll that asked for it.
    if (ask?.what === 'dispatch' && ask.call !== undefined) {
      const wanted = ask.call;
      const at = turnOfDispatch(held.turns, wanted);
      if (at !== null) {
        asking = false;
        list?.scrollToIndex(at, { align: 'start' });
        // The row mounts with the turn, and a tall turn takes real time to
        // draw: the chase runs on a wall-clock budget rather than frames, so
        // a slow mount is not mistaken for a dispatch that is not there.
        const until = Date.now() + 2500;
        // A revealed row does not stay where it was put: the turn it lives in
        // has just mounted with estimated heights and corrects itself as it
        // measures, and the row's own open grows it. Both move the row AFTER
        // the reveal's scroll - the reproduced "first click random, second
        // click right". Re-place it, without re-opening or re-flashing, until
        // it holds still.
        //
        // RECORDED ACCEPTANCE, not a pinned test: this loop and the anchor
        // re-capture ride real layout - the virtualiser measuring, the row
        // growing on open - and jsdom performs no layout, so the suite cannot
        // distinguish it from a no-op. The class it leaves open is the
        // landing's exactness under post-mount movement; the acceptance is
        // Ved clicking a cold row on the live page (it did, and the fix is
        // what he signed off), and the reintroduction would read exactly as
        // "first click random, second click right" did. The unit-level half
        // IS pinned: reveal's instant, nearest argument in subagents.test.
        const settle = () => {
          if (viewport === null) return;
          const row = callRow(wanted, viewport);
          if (row === null) return;
          const box = row.getBoundingClientRect();
          const seen = viewport.getBoundingClientRect();
          // Only when the settle moved the row clean out of view: a small
          // drift is left alone, because re-centring it here is the second
          // step the reveal just stopped taking.
          if (box.bottom < seen.top || box.top > seen.bottom) {
            row.scrollIntoView({ behavior: 'auto', block: 'nearest' });
            if (captureAnchor()) owed = null;
          }
        };
        const chase = () => {
          // The reveal scrolled: re-capture the anchor HERE, so the column's
          // own restorer (which runs on the layout mutations the reveal's
          // open causes) holds this new place rather than pulling the reader
          // back to the row they left.
          if (reveal(wanted)) {
            if (captureAnchor()) owed = null;
            // The first pass waits a beat for the mount's own measurements to
            // land; the rest catch whatever settled behind them.
            const settleUntil = Date.now() + 1000;
            const hold = () => {
              settle();
              if (Date.now() < settleUntil) setTimeout(hold, 120);
            };
            setTimeout(hold, 400);
            return;
          }
          if (Date.now() < until) setTimeout(chase, 60);
        };
        setTimeout(chase, 0);
      } else if (held.cursor === null) {
        // The real top: the dispatch is not in this conversation at all.
        asking = false;
      } else {
        // Keep asking while pages land: `older()` answers false for a fetch
        // already in flight as well as for the top, and treating that as the
        // top is what made an older dispatch's click do nothing at all. The
        // empty-page beat is a third false - a chase that stops on it stands
        // the click down until the beat passes, rather than spinning.
        loadOlder(MORE_TURNS);
      }
      return;
    }
    const at = latestCompaction(held.turns);
    if (at !== null) {
      asking = false;
      list?.scrollToIndex(at, { align: 'start' });
      return;
    }
    if (!loadOlder(MORE_TURNS)) asking = false;
  });

  // A reader at the end FOLLOWS the newest turn: that is what the end of a
  // conversation means, and a page that grew without the view moving would
  // lose the very thing it was opened on. A reader anywhere else is left
  // exactly where they are - the state is the reader's own, and the two
  // transitions that set it are the two below.
  $effect(() => {
    if (follows === null || !held.loaded || !held.following) return;
    land();
    // A landing while the reader is at the latest can take the held set over
    // the window, and this is the place that knows what is on screen: the
    // window settles here rather than in the conversation, which cannot see
    // whether the column fits.
    settleWindow();
  });

  /**
   * **A conversation that does not fill the column has no top to scroll to.**
   * The reach ask rides a scroll event, and a column whose whole held window
   * fits its room never fires one - so a seat that opens on a page with
   * history above it could not page back at all until a turn grew enough to
   * overflow (#2139). The fitting state is its own trigger: while everything
   * held fits the room and the walk has somewhere to go, ask for the page
   * above; every landing publishes the record, so the check runs again and
   * the fill continues until the column overflows or the history ends.
   *
   * **Nothing is read before there is a layout to read**: an unmeasured list
   * reports no height, which reads as fitting, and a fill asked on that would
   * take a page it does not need.
   */
  function fillFittingRoom(): void {
    // **A seat being entered pays no page fetch** (`entering`): the fit is
    // read again once the entry's own draw has painted.
    if (entering) return;
    if (viewport === null || viewport.scrollHeight === 0) return;
    // **A refused conversation is its own retry's to recover.** A refusal
    // publishes; without this the fill re-asks on every refusal at the frame
    // cadence, where `retryAsk` already owns the recovery (#2411).
    if (!held.loaded || held.refused !== null) return;
    // A page that repeats the cursor it was asked with is the walk's own
    // stop; the fill stops on it too. **The mark is the conversation's**,
    // cleared where its ask bookkeeping is cleared - so a refusal, a dropped
    // socket and a swap all free the fill, and an empty page leaves the
    // beat's own timer pacing the same cursor's retry.
    if (held.cursor !== null && held.cursor === held.askedFor) return;
    // Above the loop, so an overflowing column pays one read, not one per
    // turn before it learns there is nothing to fill.
    if (!holdsEverything()) return;
    // **Every held turn has to have reported its height.** A list still
    // drawing its rows reads short, and one read short takes a page it does
    // not need; a window that fits is a window whose every row was drawn, so
    // the measured set is the honest test of "there is nothing left to
    // scroll". A held row draws nothing and reports nothing until its drain,
    // so it is skipped.
    for (const turn of held.turns) {
      if (turn.held === true) continue;
      if (rowSizes.sizeOf(turn.key) === undefined) return;
    }
    loadOlder(pageTurns());
  }

  /**
   * The events the fill runs on: every landing and a refusal.
   * **Not every published frame**: a seat emits on every frame it folds, and
   * the fill has nothing to say about most of them.
   *
   * **The landing half is a COUNT, not the cursor.** A reconnect's newest page
   * can answer with the very cursor and turns the record already held, so a
   * key built from those fields woke for neither the drop nor the page that
   * made the history answerable again - the mark's fourth missed edge, as a
   * trigger (#2411). `pages` moves on every landing.
   *
   * **Neither the cursor nor the seat belongs here.** A return to a kept
   * conversation asks its newest page on the way in (`showing`), so the
   * column is never handed a conversation to draw with no landing behind it;
   * a seat swapped for a fresh one arrives as the empty edge.
   */
  const fills = $derived(held.loaded ? `${held.pages}:${held.refused ?? ''}` : '');

  /**
   * The frame-deferred check, run on the edges above.
   *
   * **The read waits a frame**, because the sizes right after a landing are
   * the layout BEFORE the rows it added are measured: a column that is about
   * to overflow reads as fitting there. The size observer below catches the
   * other half - the layout settling - so a check that waits here and finds
   * nothing still gets one when the rows report.
   *
   * **An empty page's beat is waited out rather than refused.** Its ask is
   * held off until the stamped beat, and a quiet seat publishes nothing to
   * wake the fill, so the fill schedules the retry itself (#2411). No mark
   * crosses the wait: the landing cleared it, and `older`'s own beat gate is
   * what holds the early checks back.
   */
  $effect(() => {
    const edge = fills;
    const element = viewport;
    if (edge === '' || element === null) return;
    return untrack(() => {
      if (held.refused !== null) return;
      if (held.reaskAt !== null) {
        // **One millisecond late on purpose.** `takePage` stamps `emptyAt`
        // and `reaskAt` from separate clock reads, so a timer computed from
        // `reaskAt` can land a fraction before `older`'s own beat gate, be
        // refused, and leave a quiet fitting seat with nothing re-armed.
        const delay = Math.max(0, held.reaskAt - Date.now()) + 1;
        const timer = setTimeout(() => fillFittingRoom(), delay);
        return () => clearTimeout(timer);
      }
      const frame = requestAnimationFrame(() => fillFittingRoom());
      return () => cancelAnimationFrame(frame);
    });
  });

  /**
   * The place a reader away from the foot is holding, put back whenever the
   * conversation changes around them.
   *
   * **The other half of the follow's pass.** That one pins the foot for a
   * reader who is at it; this one holds the row for a reader who is not.
   *
   * **Scheduled only when the rows above the anchor REORDERED** (#1734). A
   * size change under them is the observer's pass - it fires post-layout and
   * the `restored` counter below guards the overlap - and a frame that moved
   * nothing schedules nothing, where a pass per frame was the per-frame rAF
   * cost the issue measured. What is left is the move no observer event
   * reports: an equal-height reorder puts the anchor row somewhere else with
   * the sizes unchanged, and the signature of the keys above it is the data
   * that says so. (The follow's own second look is gone for the same reason:
   * at the foot an equal-height move changes nothing, and any size change is
   * the observer's.)
   *
   * **Skipped while the prepend compensation is on**: that path holds the
   * reader by the list's own compensation as older turns arrive above them,
   * and two hands on the scroll is one too many.
   */
  $effect(() => {
    const park = held;
    const moving = compensating;
    if (anchor === null || park.following || !park.loaded || moving) {
      // No anchor parked, nothing owed: the next parked run schedules against
      // a clean slate rather than against an order from before the episode.
      owed = null;
      owedUnplaced = false;
      return;
    }
    const above = keysAbove();
    // **A null signature is a pass too**, because the anchor row can be gone
    // rather than merely out of the drawn window: the fold can re-key a row in
    // place, and without a pass the lookup would miss for every landing after
    // it. The restore re-captures on the miss (#1834). **An unplaced pass
    // retries even at an unchanged signature**, or a stamp with nothing under
    // it would swallow every later publish.
    if (above === owed && !owedUnplaced) return;
    const want = above;
    const seen = restored;
    const settled = requestAnimationFrame(() => {
      // The debt is paid HERE, by the pass that runs - so a teardown's cancel
      // before the paint leaves it standing for the next run to re-arm.
      if (restored === seen) restoreAnchor();
      if (!owedUnplaced) owed = want;
    });
    return () => cancelAnimationFrame(settled);
  });

  /**
   * The ordered keys of the drawn rows above the anchor, as one signature -
   * or `null` when the anchor row is not drawn at all.
   *
   * Read off the document rather than the model because the fold decides the
   * order - the rows are what the reader sees move - and attribute reads
   * force no layout, unlike a box. **`null` rather than a slice running to
   * the window's end**: an anchor out of the drawn window has nothing for a
   * pass to restore, and the window's own churn (rows entering and leaving it
   * as the reader scrolls) is not a reorder.
   */
  function keysAbove(): string | null {
    if (viewport === null || anchor === null) return null;
    const keys: string[] = [];
    for (const row of viewport.querySelectorAll('[data-k]')) {
      const key = row.getAttribute('data-k') ?? '';
      if (key === anchor.key) return JSON.stringify(keys);
      keys.push(key);
    }
    return null;
  }

  /**
   * Where the previous scroll event left the reader, so an event's own
   * DIRECTION is readable - which is what tells the reader's return from a
   * clamp at the foot.
   */
  let movedFrom = 0;

  /** Where the reader is, and whether they have reached the top. */
  function scrolled(offset: number): void {
    // The event's own direction, read before any branch moves on: a scroll
    // that came down toward the foot is the reader's return, and it arms no
    // up-capable wheel token at all - Ved's own gesture is "scroll up and
    // come back down" (2026-10-11). Every event updates the tracker, the
    // pin's echo included: a pin is a write like any other move.
    const towardFoot = offset > movedFrom;
    movedFrom = offset;
    // **An event at the pin's own landing spot is its echo, and reading the
    // layout back for it is the per-frame cost issue #1710 measured.** A
    // write is a scroll, so every pin comes back as an event the reader did
    // not cause; the token is consumed once, and a write of the value it
    // already holds sets none at all, so a reader's own scroll to that
    // offset later is read like any other.
    if (pinEcho !== null) {
      const written = pinEcho;
      // Consumed OR cleared by every event: only the event that comes back
      // immediately is the echo, and anything else the reader did in between
      // takes the token away - a real scroll to the same offset later is
      // read like any other.
      pinEcho = null;
      if (offset === written) {
        // **The echo still carries the reach ask.** No reader moved, so none of
        // the reader-state machinery below is the echo's to run - but a landing
        // that puts the foot inside the near-top reach is an event the ask rode
        // in the base app, and a history shorter than the room would otherwise
        // never ask for what is above it. **The entry's own pin echo is the
        // one exception** (`entering`): it is the machine's event, not a
        // reader's, and the lift runs the ask it refuses.
        if (offset < REACH && !entering) loadOlder(pageTurns());
        return;
      }
    }
    // **The very end, with no reading threshold.** A reader a few pixels short
    // of it is mid-line, and a tolerance here is a column that moves under them
    // - the clamp in the view this client replaced re-engaged its follow only
    // at `scroll_offset >= max_scroll` for exactly this reason.
    //
    // Nothing is owed to arithmetic either: both sides are the element's own
    // numbers, and the clamp makes them meet at the foot - swept over whole
    // and fractional heights at device pixel ratios 1 to 3, the gap is 0 in
    // WebKit and Chromium alike (CSS `zoom` past 1 is the one divergence
    // found, and nothing here zooms the column) - so a tolerance could only
    // re-arm the follow for a reader who has moved off it.
    //
    // **And only a reader who has moved may switch it off.** A pin fires a
    // scroll event of its own, and the foot can settle past the height one
    // asked for: both read as the reader back above the foot with nothing
    // moving them, and disarming there is a column stuck where it opened.
    //
    // **And a size change is not a reader moving either, which is the harder
    // half.** A row corrected to its drawn height takes height out of the
    // column, the browser clamps the reader down with the content, and THAT
    // fires a scroll event landing at the foot - so a resize that re-armed the
    // follow through the observer re-armed it here as well, and a reader who
    // scrolled away was carried back by whatever arrived. The foot at the last
    // event is what tells the two apart, and the rule is the next paragraph's.
    //
    // **Only a foot coming UP can be a clamp.** Content arriving moves the
    // foot away and leaves the reader exactly where they are, so an event
    // after one is the reader's own; the foot closing on them - content
    // corrected shorter, or the column taking room as the composer below
    // collapses - is the browser moving them rather than them moving.
    const height = viewport?.scrollHeight ?? 0;
    const room = viewport?.clientHeight ?? 0;
    const shrank = height < lastSeen.content;
    const rose = height - room < lastSeen.content - lastSeen.room;
    lastSeen = { content: height, room };
    const foot = atFoot();
    const byReader = readerMoved();
    const aimed = aimedAtFoot();
    if (foot) {
      // **A return that lands at the foot re-arms, whatever the size did.**
      // The clamp skip exists for a foot that closed on the reader, and their
      // down-scroll can carry one in the SAME event - a row settling from the
      // estimate to its drawn height corrects the content shorter exactly as
      // their scroll lands - so the re-arm was swallowed and, with nothing
      // left to fire another event, the way-back arrow stuck at the foot until
      // pressed (Ved, 2026-10-11, "even if there is nothing to scroll"). Three
      // things make such an event theirs: a hand heard just before it either
      // way - an up gesture, a touch or a bar press, or a down gesture, which
      // aims at the foot and arms no up-token - or the event's own direction,
      // the offset moving TOWARD the foot, which is his gesture ("scroll up
      // and come back down", a down-scroll that can fire after the window has
      // closed). A clamp that moved them nowhere or away stays the layout's,
      // and the away-from-foot branch is untouched.
      if (!rose || byReader || aimed || towardFoot) {
        working?.following(true);
        // The reader is back at the latest: the older stretch behind them is
        // what the window gives up.
        settleWindow();
        // The foot is where a following reader wants to be, so the place they
        // held on the way there is done with.
        anchor = null;
      }
    }
    // `placed` is where the last pin left the reader; before any pin has
    // run it is unknown, and a reader above the foot is above it whatever
    // that number is - so the comparison falls back to any upward move
    // rather than never disarming, which left the way-back hidden on a
    // column that had not pinned yet (Ved, 2026-10-03).
    //
    // **And an event nobody made may not switch the follow off.** A
    // re-measure nudges the scroll by a couple of pixels with no reader
    // behind it, so a move inside `DISARM_SLACK` counts only when the
    // reader's own wheel, touch or key was heard just before it; their
    // moves count at any distance.
    else if (!shrank || byReader) {
      const slack = byReader ? 0 : DISARM_SLACK;
      if (offset < (placed ?? Infinity) - slack) working?.following(false);
    }
    // **A reader away from the foot has a place, and this is where it is read.**
    // Their own scroll is the one moment the page is where they put it, so the
    // row under their top edge is what the column holds their place by from
    // here on. Read off this event's own arithmetic rather than the record:
    // the record's follow flag is the fold's, this disarm publishes at once,
    // and a STREAM fold that engages the follow - a prompt frame - lands a
    // painted frame later - so a capture that read the record would depend on
    // which side of that timing it caught.
    if (!foot && captureAnchor()) owed = null;
    if (offset < REACH) loadOlder(pageTurns());
  }

  /**
   * How many turns the reader's own pull asks for: a screen and a half of
   * this column's own median turn.
   *
   * **Measured, not assumed**: the median is what the column has watched its
   * rows measure, so the same rule gives three turns on a seat of long
   * transcript reads and eleven on a seat of short ones - and the landing
   * jumps by about a screenful either way. The ceiling and the floor are
   * both the ask's (`Chat#ask`), which every caller crosses: this returns the
   * measured size and leaves the wire's limits in one place.
   */
  function pageTurns(): number {
    const median = rowSizes.median();
    const room = viewport?.clientHeight ?? 0;
    if (median === undefined || room <= 0) return MORE_TURNS;
    return Math.round((room * PAGE_VIEWPORTS) / median);
  }

  /**
   * Ask for the turns above, holding the reader's place while they arrive.
   *
   * **The count is the caller's**: the reader's own pull is sized by what the
   * column has measured (`pageTurns`), while the two walks - the dispatch
   * chase and the compaction jump - keep the protocol's full page, because
   * their pages land on the way to a place the reader asked to go and a small
   * page would turn a far jump into dozens of round trips.
   *
   * **The count here is only what the loading line and the restore read.** The
   * list is not handed it: the compensation belongs to the landing, the change
   * that moves the head, and it comes with that change (see `shift`). An ask
   * that never went arms nothing, because no change follows it.
   */
  function loadOlder(turns: number): boolean {
    if (working?.older(turns) !== true) return false;
    outstanding += 1;
    return true;
  }
</script>

{#if (waking || spawning) && held.turns.length === 0}
  <!-- Each state below is the column in that state, so each carries the
       column's own rules: without them the copy is the one thing on the page
       drawn at no padding and no gutter, while the list beside it is not. -->
  <div class="conv">
    <!-- **Every narrative hold is the composer band's**, one draw with only
         the text changing: a seat coming up says so there, on either side of
         the roster noticing (a spawn the core is running, or the lead's own
         this page dispatched), and a worker's seat with no spawn coming says
         `not running` there too. This column draws nothing for either state.
         **A wake with history in hand draws the history instead** (Ved,
         2026-10-12): the server answers a sleeping seat's own transcript, so
         the page lands while the spawn is still in flight - and the waking
         line is for a seat with nothing to draw yet, not a cover over a
         conversation already loaded, which is why this branch is empty-turns
         only. -->
  </div>
{:else if held.refused !== null && held.turns.length === 0}
  <!-- A refusal with nothing drawn under it is the column in that state. With
       turns held it is a line above them instead, because the refusal belongs
       to the ask rather than to the conversation - replacing the list would
       take the reader's own history away and unmount the thing whose scroll
       asks again. -->
  <div class="conv">
    <p class="hold off">This forge would not answer for this conversation: {held.refused}</p>
    {#if echo !== undefined}
      <Echo {echo} onretry={retry} />
    {/if}
  </div>
{:else if !held.loaded}
  <!-- Connected, and the first page has not come back. **The band says the
       wait** - every narrative state speaks there - so the column draws no
       line of its own; the reader's own words still ride here, because a send
       whose row is nowhere is a send the reader cannot see. -->
  <div class="conv">
    {#if echo !== undefined}
      <Echo {echo} onretry={retry} />
    {/if}
  </div>
{:else if held.turns.length === 0}
  <div class="conv">
    {#if echo !== undefined}
      <!-- A seat with no history still has a message on its way, and that row
           is the first thing it says: the empty copy would claim nothing was
           said while the reader watches their own words arrive. -->
      <Echo {echo} onretry={retry} />
    {:else}
      <div class="hold">
        Nothing said yet
        <span class="sub">this seat has no history: what is said here starts it</span>
      </div>
    {/if}
  </div>
{:else}
  {#if held.refused !== null}
    <!-- The server's own words for the ask that failed, said beside the rows
         it did not replace: the list stays mounted, so the reader keeps their
         place and their next scroll to the top asks again. -->
    <p class="refused">This forge would not answer for this conversation: {held.refused}</p>
  {/if}
  <!-- The list draws its own scroll viewport, so the sheet's `.conv` rules go
       on that element rather than a wrapper around it: they are the column's
       padding, its scrollbar gutter and its scrollbar, and a wrapper would put
       them outside the thing that scrolls. **`display:flex` rides here as an
       inline style on purpose**: the list's own viewport style sets
       `display:block` inline (composed before this one, so this wins), and a
       class cannot beat it - without the flex column the foot anchor's auto
       margin resolves to zero on every real conversation. -->
  <VList
    class="conv"
    style="display:flex;flex-direction:column"
    data={held.turns}
    getKey={(turn: HeldTurn) => turn.key}
    itemProps={() => ({
      // **A row the list has not measured is drawn, not hidden.** virtua
      // paints the slot and sets `visibility: hidden` until a row reports its
      // height, which is the black a fast scroll meets (#2398). The style
      // rides after virtua's own, so the row is drawn at its natural height
      // on the offset the list's estimate placed it at, and the measurement
      // corrects the offsets. The foot's pin and the parked anchor both
      // absorb that correction.
      style: { visibility: 'visible' },
    })}
    {shift}
    itemSize={ROW_ESTIMATE_PX}
    onscroll={scrolled}
    {@attach scrollViewport}
    bind:this={list}
  >
    {#snippet children(turn: HeldTurn)}
      <!-- `data-t` names the wrapper for the turn it draws, which is how the
           foot's reservation check finds the newest row's box - the row's own
           keys ride its units, and a held turn has none. -->
      <div
        class="turn"
        data-t={turn.key}
        style:min-height={turn.held ? heldReserve(rowSizes.sizeOf(turn.key)) : undefined}
        {@attach (node: HTMLElement) => measureTurn(node, turn.key)}
      >
        <Turn
          {turn}
          {slot}
          turns={held.turns}
          {stops}
          carried={turn.key === newest ? (pinned?.key ?? null) : null}
          onreadoutput={readOutput}
        />
        <!-- The echo rides the newest row, which is where the words will land:
             the row it is drawn in is the one the core's own copy opens or
             joins, so nothing moves when the send is taken. -->
        {#if echo !== undefined && turn.key === newest}
          <Echo {echo} onretry={retry} />
        {/if}
      </div>
    {/snippet}
  </VList>
  <!-- An older page in flight, said at the column's top edge: the wheel is
       clamped at the top for as long as the ask takes, and with nothing drawn
       the column reads as a dead scroll rather than as one waiting on a
       page (#2398). It stands over the rows and takes no pointer. -->
  {#if outstanding > 0}
    <div class="older-loading" role="status">Loading older messages…</div>
  {/if}
  <!-- The way back to the foot, shown ONLY while the reader is away from it:
       following means the newest row is on screen, so its presence is the
       state read at a glance and its click is the whole way back - at the
       foot and following again, in one move. -->
  {#if !held.following}
    <button
      class="follow"
      type="button"
      title="back to the latest"
      onclick={() => {
        working?.following(true);
        settleWindow();
        anchor = null;
        land();
      }}
    >
      <Icon name="down" />
    </button>
  {/if}
{/if}
<!-- The waiting prompts stand in EVERY conversation state, not only under the
     list: a seat coming up, refused, not yet read or empty holds the same tree,
     tasks and watchers as one mid-turn. The strip and the box are one surface
     now, drawn by the composer - the column publishes its pinned row through
     `pinnedRow` and nothing else stands at this foot. -->
{#if queue !== undefined}{@render queue()}{/if}

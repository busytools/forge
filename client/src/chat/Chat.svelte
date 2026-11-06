<script lang="ts">
  import { untrack, type Snippet } from 'svelte';
  import { VList, type VListHandle } from 'virtua/svelte';
  import { heldReserve, sizes } from './row-sizes';

  import Icon from '../components/Icon.svelte';
  import { cannotPaint } from '../paint';
  import {
    noteConversationOpened,
    noteDrawn,
    notePainted,
    noteScrollMove,
    type DrawnGeometry,
  } from '../perf';
  import { MORE_TURNS, subjectKey } from '../protocol';
  import { scrollAsk } from '../session/scroll-ask';
  import type { Connection } from '../socket';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import { anchoredScroll, heldRow, type Anchor, type RowBox } from './anchor';
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
  import { conversationsFor, places, type Place } from './conversations';
  import Echo from './Echo.svelte';
  import { glideTo as glideToward, wantsMotion } from './glide';
  import { echoes, ownWords } from './echoes.svelte';
  import { pinnedRow } from './pinnedRow.svelte';
  import Turn from './Turn.svelte';
  import { turnOfDispatch } from './dispatch-jump';
  import { callRow, reachableIds, reveal, subagents } from './subagents.svelte';
  import { stopsOf, turnFold, type PageStops, type TurnInfo } from './units';

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
  /** How long a row's arrival mark stays on it, past the longest animation. */
  const ARRIVE_MS = 340;
  /** The longest a return's hold waits for a paint before it lifts anyway. */
  const HOLD_MAX_MS = 250;
  /** How many frames a restored place waits for its row to draw before giving it up. */
  const RESTORE_TRIES = 3;
  /** How many frames a capture after a jump waits for the layout to settle. */
  const CAPTURE_FRAMES = 6;
  /**
   * How long a restore keeps asking the list for a turn it has not drawn yet.
   *
   * The row's panel is the list's ONE item, and a large one - a long turn or a
   * live one - moves when the list measures it after a jump. Frames are the
   * unit the row is re-drawn on and milliseconds are the unit a mount is slow
   * in, so this is the budget for the second, not a retry of the first.
   */
  const RESTORE_MS = 1200;
  /** How far above the last pin counts as the reader when no input preceded it. */
  const DISARM_SLACK = 48;
  /** How far above the very end still counts as standing at the foot. */
  const FOOT_NEAR = 48;
  /**
   * How far short of its target a scroll read-back may land.
   *
   * **Browsers round `scrollTop` to a fraction of a pixel**, so a pin written
   * at the list's fractional model foot can read back below it - which is
   * still the foot, not a reader who moved.
   */
  const FOOT_QUANTUM = 1;
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
  /**
   * The list mount a forced re-measure replaces (#2825).
   *
   * **`virtua` has no measure call.** Its range stays empty until its own
   * ResizeObserver reports the viewport, so a delivery that never lands is a
   * mounted list drawing zero rows; a fresh mount re-registers that observer,
   * which is what leaving the seat and coming back did by hand.
   */
  let listEpoch = $state(0);
  /** Whether this open already forced one; a second would be the loop. */
  let remeasured = false;
  /** The frame a measurement check is waiting on, held so one schedule is outstanding. */
  let checkFrame: number | null = null;
  /** Where the column last left the reader, which its own pin's echo cannot disarm. */
  let placed: number | null = null;
  /** The seat `placed` was recorded on, so a re-run for the same seat keeps it. */
  let placedFor: string | null = null;
  /**
   * Whether this open has already reported its first painted row. The probe
   * keeps one per open; this keeps the turn lookup that feeds it off the
   * resize path once the record is written.
   */
  let paintNoted = false;
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
  /**
   * A place a give-up kept because the loaded window had not reached its row.
   *
   * The give-up still lands the reader at the latest, and that landing's pin
   * echoes back through the scroll handler with no anchor - a state that
   * normally means the reader gave their place up. This is what tells the
   * two apart, and it goes on the reader's own next move or return to the
   * foot.
   */
  let placeKept = false;
  /**
   * The place a returning reader is being put back on, until the list draws
   * the row it names.
   *
   * **A fresh list mounts at its start**, so the row may be outside the drawn
   * window while this is set: the captures and the parked passes stand aside
   * until the scroll to its turn has drawn it, or the column would take the
   * top of the window as the reader's place.
   *
   * It carries the session the place was recorded in as well as the row: a
   * connection that opens a conversation before its occupant's id is known
   * still takes the saved place, and the id that arrives decides whether it
   * was this conversation's.
   */
  let restoring = $state<Place | null>(null);
  /** The frame the return's own scroll waits on, held so one schedule is outstanding. */
  let restoreFrame: number | null = null;
  /**
   * The claude session this column last saw on its seat, or null until one is
   * named.
   *
   * **A different id is a different conversation**, and the column stays
   * mounted across a `/new` or a `/resume`: the reader's row and their parked
   * state belong to the occupant that left, and the successor opens at its
   * latest the way a fresh entry does. A replacement that names the SAME id
   * is the same conversation reloading, and nothing is reset for it.
   */
  let shownSession: string | null = null;
  /** How many frames a restored place has waited for its row to draw. */
  let restoreTries = 0;
  /** When the restore being placed began, against [`RESTORE_MS`]. */
  let restoreStarted = 0;
  /**
   * Whether the entry's placement has settled.
   *
   * **The open places its reader; only a settled column follows.** While the
   * list's model is still catching up with the rows the mount drew, the foot
   * moves under a reader who has just arrived - holding at the model or
   * gliding to it walks them down the conversation. Every pass until the
   * model reaches the element's own end is the placement.
   */
  let openPlaced = false;
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
   * The row keys each turn has already shown, by the row's own `data-k`, and
   * the turns seen at all.
   *
   * **The row's key, not the DOM's shape.** The list re-creates a row's node
   * whenever it leaves and re-enters the drawn window, and a fold rebuild
   * re-creates it in place - so a mark read off the wrapper's last child
   * replays the entrance for rows the reader has already seen, and the same
   * row can enter several times inside one turn. The key is the one name a
   * row keeps across all of that, so a key seen once is never marked again.
   *
   * **A turn drawn whole is seeded from the MODEL, not from DOM nodes**: the
   * list renders its first items after the column attaches, so an attach-time
   * scan of the DOM sees nothing - and a return into a seat whose newest turn
   * was built by frames would otherwise enter every row of it at once.
   */
  let arrived: Record<string, Record<string, true>> = {};
  let seenTurn: Record<string, true> = {};

  /** Whether this row has arrived in this turn before. */
  function hasArrived(turn: string, key: string): boolean {
    return arrived[turn]?.[key] === true;
  }

  /** Record a row as arrived, keeping the record to the turns the column holds. */
  function recordArrival(turn: string, key: string): void {
    let rows = arrived[turn];
    if (rows === undefined) {
      rows = {};
      arrived[turn] = rows;
    }
    rows[key] = true;
    // **Bounded to the turns the column holds**, on every insert: a turn the
    // window no longer draws cannot be drawn again, so its keys go with it.
    // The turn being recorded is kept even when `held` is still the outgoing
    // conversation's - seeding a switch happens before the new record is drawn.
    const drawing: Record<string, true> = { [turn]: true };
    for (const turn of held.turns) drawing[turn.key] = true;
    for (const drawn of Object.keys(arrived)) {
      if (drawing[drawn] !== true) delete arrived[drawn];
    }
    // The turn record is bounded the same way: a turn the column no longer
    // holds cannot become the newest again.
    for (const seen of Object.keys(seenTurn)) {
      if (drawing[seen] !== true) delete seenTurn[seen];
    }
  }

  /**
   * Every drawn row key of one turn, as the fold names them.
   *
   * The DOM's keys are the fold's: a unit or the reader's own block is
   * `<turn>:<unit key>`, and a call row is `call-<call key>` under its unit.
   * The leaf kinds without a row of their own, and cards, carry none.
   *
   * **The stops are the fold's own argument**: a stop the page holds lands on
   * the row it ends, so the same turn keys differently with and without them,
   * and the seed has to name what the render will draw.
   */
  function drawnKeys(turn: HeldTurn, stops: PageStops): string[] {
    const keys: string[] = [];
    const units = turnFold(turn, slot, beingWritten(turn), !beingWritten(turn), stops);
    for (const unit of units) {
      keys.push(`${turn.key}:${unit.key}`);
      if (unit.kind !== 'leaves') continue;
      for (const row of unit.rows) if (row.tag === 'call') keys.push(`call-${row.key}`);
    }
    return keys;
  }

  /** Record a turn already on screen as arrived, whatever built it. */
  function seedDrawn(turn: HeldTurn | null, stops: PageStops): void {
    if (turn === null) return;
    seenTurn[turn.key] = true;
    for (const key of drawnKeys(turn, stops)) recordArrival(turn.key, key);
  }

  /**
   * A turn seen for the first time.
   *
   * **How it first arrives decides.** A frame-built turn is about to draw its
   * first row, and that row is an arrival; a turn a page drew is already on
   * the page, so its rows are recorded before they reach the DOM and open
   * still. A page that merges onto a turn being written is neither - the turn
   * is already seen, and the rows it carries are ones the reader never saw,
   * so they enter.
   */
  function firstSight(
    turn: HeldTurn | null,
    turns: readonly { messages: readonly unknown[] }[],
  ): void {
    if (turn === null || seenTurn[turn.key] === true) return;
    if (turn.live) {
      seenTurn[turn.key] = true;
      return;
    }
    // **The stops are walked only where they are used.** They are the render's
    // own fold argument, and the live path must not pay a second page walk per
    // frame - which it would if the caller evaluated them before these guards.
    seedDrawn(turn, stopsOf(turns));
  }

  /** Mark one row as having just arrived, and take the mark back after it. */
  function enter(row: HTMLElement): void {
    row.classList.add('arrive');
    // **The mark goes when the LAST animation ends**, not when the shorter
    // one does: a fade outlasting the rise would be cut short by a clear
    // keyed on the rise. Both are awaited, and the timer is the fallback for
    // a node a stilled animation never reaches.
    let timer: ReturnType<typeof setTimeout> | null = null;
    let sawFade = false;
    let sawRise = false;
    const clear = (event: AnimationEvent): void => {
      if (event.animationName === 'arrive-fade') sawFade = true;
      else if (event.animationName === 'arrive-rise') sawRise = true;
      else return;
      if (!sawFade || !sawRise) return;
      row.classList.remove('arrive');
      row.removeEventListener('animationend', clear);
      if (timer !== null) clearTimeout(timer);
    };
    row.addEventListener('animationend', clear);
    timer = setTimeout(() => {
      row.classList.remove('arrive');
      row.removeEventListener('animationend', clear);
    }, ARRIVE_MS);
  }

  /**
   * Every `[data-k]` key one inserted node brought into this turn.
   *
   * **The turn's own `data-t` decides whose rows these are**, here rather
   * than again in `rowsIn`: a node that brought no key of this turn is not
   * this turn's insertion at all.
   */
  function keysIn(node: Element, turn: string): string[] {
    const out: string[] = [];
    for (const row of node.querySelectorAll('[data-k]')) {
      if (row.closest('[data-t]')?.getAttribute('data-t') !== turn) continue;
      const key = row.getAttribute('data-k');
      if (key !== null) out.push(key);
    }
    if (node.matches('[data-k]') && node.closest('[data-t]')?.getAttribute('data-t') === turn) {
      const own = node.getAttribute('data-k');
      if (own !== null) out.push(own);
    }
    return out;
  }

  /**
   * The rows one inserted node brought with it, outermost only.
   *
   * A new unit and the call row inside it arrive together, and entering both
   * would run the entrance inside itself; the outermost row is the one the
   * reader sees arrive. A row inserted inside a unit already drawn is its own
   * outermost, so a call joining a run enters on its own. Which turn's rows
   * these are was already answered by `keysIn`, whose filter is the one guard.
   */
  function rowsIn(node: Element): HTMLElement[] {
    if (node.matches('[data-k]')) {
      return [node as HTMLElement];
    }
    const out: HTMLElement[] = [];
    for (const row of node.querySelectorAll('[data-k]')) {
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
    // The retry is this client's own send like the press that failed: the id
    // is remembered, so it carries a parked reader to the foot rather than
    // counting in the badge.
    echoes.remember(seat, id);
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
  let pinEcho: { at: number; foot: boolean } | null = null;
  /**
   * Whether the reader's own place was the foot when the column last heard
   * from them.
   *
   * **The pin's one condition, besides the reader's own send.** The follow
   * flag says the reader wants the newest end; this says they are standing at
   * it, so an arrival may carry them with it. It goes false on the reader's
   * own move away and comes back only where a pin or a scroll leaves them at
   * the end - so a follow re-armed by an arrival holds a parked reader where
   * they are instead of yanking them down.
   *
   * Reactive, because the way-back control is drawn from it: a reader whose
   * follow is on but whose pin cannot fire needs the door back.
   */
  let footHeld = $state(true);
  /**
   * How many turns have landed since the reader parked, which the circle
   * carries while it is drawn.
   *
   * **Counted against the turn the reader parked after**, not against a
   * length: a prepend above them moves the length without moving their foot,
   * and a page that merges repeated turns shrinks it again. A turn is counted
   * once however many frames stream it, because only a new TURN moves the
   * index, never a frame joining the one already there.
   */
  let badge = $state(0);
  /** The newest turn's key at the moment the reader parked; null when following. */
  let badgeFrom: string | null = null;
  /** What cancels the glide in flight, the foot it aims at, and its last write. */
  let glideStop: (() => void) | null = null;
  let glideTarget: number | null = null;
  let glideAt = 0;
  /**
   * Whether the reader is away from the foot, which is what the circle and
   * its badge answer to.
   *
   * **Not the follow flag alone.** The follow can be on with its pin unable
   * to fire (the third state the way back and the observer's restore already
   * serve), and that reader is still parked: they are owed the count of what
   * landed while they read.
   */
  const parked = $derived(!held.following || !pinning());

  /** Stop the glide in flight, if any, without touching where it reached. */
  function stopGlide(): void {
    if (glideStop !== null) glideStop();
    glideStop = null;
    glideTarget = null;
  }

  /**
   * Glide the viewport to `target`, a foot of `reachable`, and settle the
   * landing when it arrives.
   *
   * **`target` is the CLAMPED destination**, the browser's own maximum, not
   * the raw `scrollHeight` the instant write asks for: a tween aimed at the
   * raw end overshoots the clamp on its first frames, and the read-back then
   * differs from the requested value by the whole viewport - which the scroll
   * handler reads as a foreign move and cancels the glide.
   *
   * **The completion owns everything the instant write's read-back owns**:
   * where the reader ended, whether that is the foot, the pin's own echo
   * token, and the place a landed foot gives up. A glide already heading for
   * the same target is left alone - the frames a stream delivers retarget it
   * only when the foot actually moved.
   */
  function startGlide(target: number, reachable: number): void {
    const box = viewport;
    if (box === null) return;
    if (glideStop !== null && glideTarget === target) return;
    stopGlide();
    glideTarget = target;
    glideAt = box.scrollTop;
    /** The offset the last glide write found, so a write that moved is known. */
    let before = box.scrollTop;
    glideStop = glideToward(
      {
        get scrollTop(): number {
          return box.scrollTop;
        },
        set scrollTop(value: number) {
          before = box.scrollTop;
          box.scrollTop = value;
          // **What the browser LANDED, not what was asked for.** A write can
          // clamp; the scroll event this write comes back as carries the
          // landed offset, so the guard that compares the two has to hold the
          // same number or it reads the glide's own frame as a foreign move.
          glideAt = box.scrollTop;
        },
      },
      target,
      () => {
        glideStop = null;
        glideTarget = null;
        const landed = box.scrollTop;
        placed = landed;
        const at = landed >= reachable - FOOT_QUANTUM;
        // **Only a write that moved comes back as an event.** A tween whose
        // last frame lands on the value the frame before it already wrote
        // must not arm the token: no echo is coming, and the next event would
        // be read as one.
        if (landed !== before) pinEcho = { at: landed, foot: at };
        footHeld = at;
        // A pin that lands at the foot is the reader leaving their place, the
        // same rule the instant landing keeps.
        if (at) {
          anchor = null;
          rememberPlace();
        }
      },
    );
  }

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
   * Whether the turn `key` names is held right now.
   *
   * **Read at measurement time, not captured with the row.** The attachment
   * does not re-run when the fold hands the column a new turn object, so a
   * held row's own closure still carries the turn it mounted with - which is
   * a `held: false` for a row that is held now.
   */
  function turnIsHeld(key: string): boolean {
    return held.turns.some((turn) => turn.key === key && turn.held === true);
  }

  /**
   * Record what a turn's row measures, so a row the send's pull-and-return
   * keeps mounted holds its own space while it draws nothing (#1890).
   */
  function measureTurn(node: HTMLElement, turn: HeldTurn): () => void {
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const height = entry.contentRect.height;
        rowSizes.remember(turn.key, height);
        // **The open's painted mark is the first row with a box** (#2733,
        // #2752). The drawn mark is the model and lands before this flush's
        // DOM exists, so only this one can say when a reader first had a row
        // to look at. The probe keeps one per open, so a list that reports
        // every row it measures writes one record in total.
        //
        // **A held row is a box with nothing in it** (#2754 review): it keeps
        // its wrapper mounted and reserves a height while it draws nothing,
        // so it is skipped rather than read as the reader's first row. The
        // lookup runs only until this open has its mark.
        if (!paintNoted && height > 0 && !turnIsHeld(turn.key)) {
          paintNoted = true;
          notePainted();
        }
      }
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
    // A fresh list mounts at its start and opens following, so its first pin
    // is the column putting the reader at the foot rather than an arrival
    // moving one. A return's list is placed by its restore before any pin can
    // move it. **Unless the reader holds a place**: a replacement reloads the
    // conversation with the same occupant, the list remounts, and the anchor
    // is what the loaded page must be held to - not the new element's start.
    footHeld = anchor === null;
    // **This mount is its own layout.** Whatever order the last element was
    // paid for says nothing about this one, so a parked reader is owed a pass.
    owed = null;
    owedUnplaced = anchor !== null;
    // **The offset a pin last wrote belonged to the element that is gone.**
    // `land()` reads a non-null `placed` as "a following column, hold at the
    // model's foot"; on a mount that is the wrong answer - the model lags the
    // rows already in the new element, and holding there lands the reader
    // above the conversation and walks them down as the measurement catches
    // up (Ved's "starts somewhere up and then scrolls down"). A same-id
    // re-spawn reloads the record and mounts a new list without a switch, so
    // the open's own write is the one that must place the reader.
    placed = null;
    // **And the settle belongs to this mount, not the one that is gone.** A
    // remount re-opens the walk `placed` above is cleared for, one pass
    // later: the first pin places, and the next holds at the lagging model
    // unless the settle is cleared with it.
    openPlaced = false;
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
    // **A hand on the column takes the viewport from the glide.** The reader's
    // own gesture wins over the finish the follow was making: the glide is
    // the column moving on their behalf, and they are moving themselves now.
    const arm = () => {
      stopGlide();
      // **The reader's own hand gives up a place a restore kept.** The latch
      // exists so a give-up's landing does not delete the place; from the
      // reader's next gesture on, the foot is theirs to give it up at.
      placeKept = false;
      // **A gesture that stops a glide leaves the reader where it reached.**
      // `placed` is the offset the disarm compares against, and a glide's
      // completion is the only other write on that path; a stale foot from
      // before a growth would let a small up-scroll during a stream fail the
      // disarm, and the next growth would carry the reader back (#1673).
      if (viewport !== null) placed = viewport.scrollTop;
      readerAt = performance.now();
    };
    const aim = () => {
      stopGlide();
      // **The reader's own hand gives up a place a restore kept.**
      placeKept = false;
      // **The down gesture leaves `placed` alone.** Only an upward event can
      // disarm, and every one of those is an up gesture that runs `arm`
      // first - so a write here decided nothing a test could see, and the
      // disarm's own comparison stays the one the last gesture or pin set.
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
      // **A returning list has not measured when this first arrives**: the
      // delivery can report an estimate that fits while the drawn rows
      // overflow, and a re-arm here would carry the reader to the foot under
      // the restore's own scroll. The restore's placer owns the entry until it
      // lands, and a list with no viewport has no fit to read (#2825).
      if (restoring === null && list !== null && list.getViewportSize() > 0 && holdsEverything()) {
        // Nothing is left to scroll, so the end is where the reader is
        // sitting, whether or not an event came to say so.
        footHeld = true;
        working?.following(true);
        anchor = null;
        rememberPlace();
      }
      // The layout has settled, so the reach ask may have no scroll to ride
      // (#2139). **The fit itself is read inside `fillFittingRoom` and not
      // here**: the empty page's beat reaches that function on its own timer,
      // without passing this wrapper, so a guard only here would let a column
      // that overflowed during the beat ask a page it does not need.
      fillFittingRoom();
      // **A follow that cannot pin is a parked reader too.** The flag can be
      // on while the column holds position, and that reader's row still moves
      // under them - so the pin's own condition decides which pass runs.
      if (held.following && pinning()) land();
      // A reader away from the foot has a place of their own, and a size change
      // is one of the two ways it moves under them. **Not while the prepend
      // compensation is on**, same as the effect's own pass: that path holds
      // the reader by the list's own compensation as older turns arrive above
      // them, and two hands on the scroll is one too many.
      else if (!compensating) {
        restored += 1;
        restoreAnchor();
      }
      // **A size change is also the retry** (#2825): a pane that had no room,
      // or a list whose measure landed after the last check, is asked again
      // now that the element has one.
      scheduleMeasureCheck();
    });
    watcher.observe(node);
    if (content !== null) watcher.observe(content);
    // **Rows enter here, once per key.** The observer is on the scroller
    // rather than on the newest turn's wrapper because the wrapper itself is
    // re-created with the list's window; a key already in `arrived` is a row
    // the reader has seen, whoever draws it again.
    /** Rows waiting for their first paint before they enter. */
    const pending: HTMLElement[] = [];
    let pendingFrame: number | null = null;
    const arrivals = new MutationObserver((records) => {
      // **A turn still being written takes arrivals.** `live` alone is not
      // that state: a seat opened mid-flight has its newest row from a page
      // (`live: false`, `running: true`) and frames join it, and those rows
      // are arrivals too - `beingWritten` is the predicate the fold and the
      // pin both use.
      const turn = untrack(() => newestTurn);
      if (turn === null || !beingWritten(turn)) return;
      const at = turn.key;
      for (const record of records) {
        for (const added of record.addedNodes) {
          if (!(added instanceof Element)) continue;
          const keys = keysIn(added, at);
          if (keys.length === 0) continue;
          // **Every key is recorded, only the outermost row is entered.** A
          // call bundled inside a new unit enters with the unit, and its own
          // key still has to be known so a later re-create of that leaf does
          // not replay.
          const entering = rowsIn(added).filter((row) => {
            const key = row.getAttribute('data-k');
            if (key === null || hasArrived(at, key)) return false;
            // **A run is keyed by its head, which a fold rebuild can move.**
            // A stop drawn alone resolves once its launch's page lands and the
            // fold drops that head row; the rebuilt run takes the next row's
            // key, and its rows were all shown already - so it is a move, not
            // an arrival.
            const inside = [...row.querySelectorAll('[data-k]')];
            return !(
              inside.length > 0 &&
              inside.every((inner) => hasArrived(at, inner.getAttribute('data-k') ?? ''))
            );
          });
          for (const key of keys) recordArrival(at, key);
          for (const row of entering) pending.push(row);
        }
      }
      // **A row pulled back into the queue before it paints never enters.**
      // A prompt sent during a turn draws its row and the queue retracts it a
      // frame or two later; the rise in between reads as a gap over a row that
      // is already leaving.
      if (pending.length > 0 && pendingFrame === null) {
        pendingFrame = requestAnimationFrame(() => {
          pendingFrame = null;
          for (const row of pending.splice(0)) if (row.isConnected) enter(row);
        });
      }
    });
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
      // A glide aimed at the element that is going must not write into it.
      stopGlide();
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
    // A return is still placing its reader: the top of the window it is
    // scrolling to is not where they were.
    if (restoring !== null) return false;
    if (viewport === null) {
      owedUnplaced = true;
      return false;
    }
    // **The row under the edge, not merely one below it.** A jump can outrun
    // the list's window: every drawn row then sits below the top edge, and
    // the first of them is no place at all (`heldRow` carries the rule).
    const landed = heldRow(
      drawnRows(),
      viewport.getBoundingClientRect().top,
      viewport.clientHeight,
    );
    if (landed === null) {
      owedUnplaced = true;
      return false;
    }
    anchor = landed;
    owedUnplaced = false;
    rememberPlace();
    return true;
  }

  /**
   * Take the reader's place again once the list has re-windowed for a jump.
   *
   * **The layout has to agree with itself before the place is written.** A
   * jump's first frames are the list correcting its estimates: a row under the
   * top edge for one frame and gone the next was never the reader's place.
   * Bounded, and only while the reader is still parked - a capture that never
   * settles leaves the anchor it had rather than writing one from rows the
   * jump left behind.
   */
  let captureFrame: number | null = null;
  function retryCapture(): void {
    if (captureFrame !== null) return;
    let tries = 0;
    /** The row the last frame held, waiting for the next to agree with it. */
    let pending: Anchor | null = null;
    const step = (): void => {
      captureFrame = null;
      if (held.following || restoring !== null) return;
      if (tries >= CAPTURE_FRAMES) {
        // **A capture that never settled is given up, not left standing.**
        // The anchor it still holds names the rows the jump left behind, and
        // the next parked pass would restore the reader to it - undoing the
        // jump they made. Clearing it makes their next move capture afresh;
        // the log carries the give-up because a stale anchor is otherwise
        // silent.
        const view = viewport;
        anchor = null;
        owedUnplaced = false;
        owed = null;
        rememberPlace();
        noteScrollMove({
          from: view?.scrollTop ?? 0,
          to: view?.scrollTop ?? 0,
          height: view?.scrollHeight ?? 0,
          room: view?.clientHeight ?? 0,
          shrank: false,
          rose: false,
          foot: atFoot(),
          reader: false,
          aimed: false,
          restore_owed: false,
          pin: false,
          name: 'capture-gave-up',
        });
        return;
      }
      tries += 1;
      const view = viewport;
      if (view === null) return;
      const at = heldRow(drawnRows(), view.getBoundingClientRect().top, view.clientHeight);
      if (at === null) {
        pending = null;
      } else if (
        pending !== null &&
        pending.key === at.key &&
        Math.abs(pending.into - at.into) <= 1
      ) {
        anchor = at;
        owedUnplaced = false;
        rememberPlace();
        owed = null;
        return;
      } else {
        pending = at;
      }
      captureFrame = requestAnimationFrame(step);
    };
    captureFrame = requestAnimationFrame(step);
  }

  /**
   * Whether `key` is a row of `turn`.
   *
   * **The row's key is the turn's own.** Rows draw `data-k` from `turn.key`
   * (`Turn.svelte`), and the page name the fold keeps in `also` belongs to a
   * row it did NOT adopt - that name is never drawn, so it can never be a
   * place's anchor. The match is the turn's own key alone.
   */
  function turnHasRow(turn: HeldTurn, key: string): boolean {
    return key.startsWith(`${turn.key}:`);
  }

  /** The held turn a row key belongs to, through [`turnHasRow`]. */
  function turnFor(key: string): HeldTurn | undefined {
    return held.turns.find((turn) => turnHasRow(turn, key));
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
    // The return's own placer owns the scroll until its row is drawn.
    if (restoring !== null) return true;
    const place = anchor;
    if (place === null || viewport === null) return true;
    const row = viewport.querySelector(`[data-k="${CSS.escape(place.key)}"]`);
    if (row === null) {
      // **A row the list has not drawn is not the reader's place.** The turn
      // may be outside the drawn window, where the viewport's top edge belongs
      // to whatever happens to be there - a capture from it would move the
      // reader's place to a row they never chose. Only a turn the list has
      // DRAWN but a row it no longer carries is the re-key a capture heals.
      const turn = held.turns.find((one) => turnHasRow(one, place.key));
      const drawn =
        turn !== undefined && viewport.querySelector(`[data-t="${CSS.escape(turn.key)}"]`) !== null;
      if (!drawn) {
        owedUnplaced = true;
        return false;
      }
      return captureAnchor();
    }
    const box = viewport.getBoundingClientRect();
    const top = row.getBoundingClientRect().top - box.top + viewport.scrollTop;
    const want = anchoredScroll(place, top);
    // **Exact, not to the nearest pixel.** A device pixel ratio above 1 draws
    // and scrolls in fractions, so a half-pixel drift is a real move of the
    // words under the reader's eye; the guard is only what stops a write of
    // the value the element already holds.
    if (Math.abs(want - viewport.scrollTop) > 0.001) {
      const was = viewport.scrollTop;
      viewport.scrollTop = want;
      // **The write's own echo is the place's, not the reader's.** A short
      // element - a returning mount before its big turn measures - clamps the
      // write to its end, and that offset reads as the foot; without the
      // token the handler re-arms the follow over the place being restored and
      // deletes it. The land path arms this the same way, from the read-back
      // because a write can clamp and only the read-back knows where it landed.
      if (viewport.scrollTop !== was) {
        pinEcho = {
          at: viewport.scrollTop,
          foot: viewport.scrollTop >= viewport.scrollHeight - viewport.clientHeight - FOOT_NEAR,
        };
      }
    }
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
   * Whether the column may pin the foot right now.
   *
   * **The reader stands at the end, or their own send is carrying them
   * there.** The follow flag alone is not enough: it can be on while the
   * column holds a reader's position, and a pin in that state is the drift
   * this rule exists to stop.
   */
  function pinning(): boolean {
    return footHeld || held.send_armed;
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
    const store = conversations();
    const { kept, live } = store;
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
      if (untrack(() => seat) === which) {
        // **Before the draw**, so a page-drawn turn is recorded as it arrives
        // rather than after its rows have landed. The stops are the incoming
        // record's, not the derived one's: `held` is still the outgoing
        // conversation's until the line below.
        firstSight(
          untrack(() => value.turns[value.turns.length - 1] ?? null),
          value.turns,
        );
        held = value;
      }
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
    // **A third way one goes is an occupant swap** (`session_replaced`). A
    // changed id resets the offset a successor's first pin must not inherit;
    // a same-id reload arms the reader's restore. Either way the swap empties
    // the conversation, so the list unmounts and the new landing re-measures
    // before any event can read the old placement. A swap that kept the list
    // mounted across it would make this stale.
    // **The same predicate is what a conversation OPEN is** (#2374): a first
    // visit, a seat reopened on another connection, or a kept seat coming
    // back on screen. Every way into a seat - a click, a deep link, a reload,
    // a reconnect, a palette move - passes through this effect, so the perf
    // log's open is marked from here rather than from a route change: a route
    // change alone is no conversation, and a board route must write nothing.
    const switched = fresh || which !== placedFor;
    if (switched) {
      // The arrival record belongs to the conversation that is going.
      arrived = {};
      seenTurn = {};
      placed = null;
      // **The badge belongs to the column, not the seat.** Its marker is a
      // turn key of the conversation that is going; reset here, so the
      // arriving seat arms its own marker before any of its turns land rather
      // than counting them from the first arrival.
      badgeFrom = null;
      badge = 0;
      // A kept place belonged to the conversation that is going.
      placeKept = false;
      // The painted mark belongs to the open that just started, not the one
      // that is going, so the column takes it again with the seat.
      paintNoted = false;
      // And so does the one forced re-measure (#2825): the list this open
      // mounts is its own, and a black one is its own to fix. A check still
      // waiting belongs to the seat that is going, so it is cancelled rather
      // than left to read the arriving seat's first frames as its failure.
      remeasured = false;
      if (checkFrame !== null) {
        cancelAnimationFrame(checkFrame);
        checkFrame = null;
      }
      // **A switch asks the arriving seat's list its own question** (#2830).
      // The column keeps one list across two seats that both hold rows, so
      // the measure is not re-run by a mount - and a range the last seat
      // froze would otherwise follow this one in.
      scheduleMeasureCheck();
      // A restore still on its way belongs to the seat that is going.
      if (restoreFrame !== null) {
        cancelAnimationFrame(restoreFrame);
        restoreFrame = null;
      }
      // And so does a capture still waiting for the layout to settle.
      if (captureFrame !== null) {
        cancelAnimationFrame(captureFrame);
        captureFrame = null;
      }
      restoring = null;
      restoreTries = 0;
      // A glide aimed at the seat that is going is the same debt.
      stopGlide();
      // The place a reader held was a row of the conversation that is going.
      anchor = null;
      // The occupant this column has seen belongs to the seat that is going
      // too, so the arriving seat's first id is an adoption, not a change.
      shownSession = null;
      // And so were the sizes and the last event, which read that element.
      lastSeen = { content: 0, room: 0 };
      movedFrom = 0;
      // **A conversation with a saved place opens at it; every other entry
      // follows the latest.** A place is the reader's row for the
      // conversation, wherever the entry came from - a document return, a
      // seat switch back, or a reconnect that built a new connection around
      // the same occupant. Two things are not that conversation's place: one
      // its claude session id has moved on from (`/new`, `/resume`), and one
      // whose row the conversation no longer holds (a compaction or a trim
      // ate it). The restore's own session comparison gives up a moved id a
      // frame later, and `Places.settle` has usually dropped it already; this
      // entry only has to keep the one that is still the conversation's.
      const arriving = untrack(() => kept.get(which));
      const turning = arriving?.turns ?? [];
      const place = places.of(which) ?? null;
      // **A conversation still loading may hold the row in the page on its
      // way**, so only a loaded one that lacks it is a stale place.
      const holdsRow = place !== null && turning.some((turn) => turnHasRow(turn, place.anchor.key));
      const reopen = place !== null && (holdsRow || arriving === undefined || !arriving.loaded);
      if (reopen && place !== null) {
        // A returning reader is parked where they left, not at the foot, so
        // the pin's own condition must not carry them down while the row is
        // being drawn back. The follow flag goes off with it, as it was when
        // the place was recorded.
        footHeld = false;
        restoring = place;
        restoreStarted = 0;
        working?.following(false);
      } else {
        // **A fresh open and a place the conversation has outlived land at the
        // latest**, so the reader is at the foot before the first pin rather
        // than an arrival moving them to one. **And so is the follow**: a
        // seat's conversation is kept, and its follow flag was kept with it -
        // so an entry that did not take a place must re-arm it. A place the
        // conversation has outlived is dropped: its row is gone, and a later
        // entry must not reopen it. The window is settled by the follow
        // effect below, which runs on the arriving conversation's own publish
        // - this call would hand the outgoing seat's row key to the incoming
        // chat's trim.
        footHeld = true;
        working?.following(true);
        if (place !== null) places.forget(which);
      }
      // **An entry places its reader; it does not travel to them.** The
      // mount's first passes correct the list's estimates, and holding or
      // gliding there walks the reader down the conversation. A switch that
      // keeps the list mounted runs no viewport attachment, so the settle is
      // cleared here as well; the shapes that remount a list clear it there.
      openPlaced = false;
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
    const keptValue = untrack(() => kept.get(which)) ?? NOTHING;
    held = keptValue;
    // **What the conversation already draws has already arrived.** A return
    // into a seat whose newest turn was built by frames would otherwise enter
    // every row of it at once, and the list renders its first items after the
    // attach, so only the model can say which rows those are. The stops are
    // the kept record's, which the derived `stops` has not caught up to yet.
    if (switched) {
      seedDrawn(
        untrack(() => keptValue.turns[keptValue.turns.length - 1] ?? null),
        stopsOf(keptValue.turns),
      );
    }
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

  /**
   * The occupant changed under the column, which is a different conversation.
   *
   * **The column does not remount for a `/new` or a `/resume`**, so the row
   * the reader held and the parked state around it survive a swap that has
   * nothing to do with them; the successor opens at its latest, the way an
   * entry does. A replacement naming the SAME id is the conversation
   * reloading, and keeps both.
   */
  $effect(() => {
    const id = held.sessionId;
    if (id === null) return;
    const before = shownSession;
    shownSession = id;
    if (before === null) return;
    if (before === id) {
      // **`held` is reassigned on every publish, so this effect runs on every
      // frame and page.** Only the publish a REPLACEMENT leaves is the
      // record emptied: arming on any same-id run would re-open the return
      // placer over a live seat - freezing a parked reader's own scroll,
      // switching their own send off and resetting the restore's budget on
      // every frame. The emptied record has no loaded turns; an ordinary
      // publish has them.
      if (held.loaded || held.turns.length > 0) return;
      // **The reload is an entry whether or not the list remounts.** The
      // record empties, but a page that lands before a render pass sees the
      // empty state leaves the old list in place - so the mount's own clear
      // never runs, `placed` and the settle survive from the conversation
      // that is going, and the follow glides the reader to the foot from
      // wherever they were. Cleared here, where the swap is observed.
      placed = null;
      openPlaced = false;
      // **A replacement naming the same id is the conversation reloading**:
      // the record empties, the list remounts, and the reader is owed the row
      // they were parked on. That row can be an older turn the new list's
      // first window does not draw, so the restore is armed here - without
      // it the column opens at the top of the reloaded page while the stored
      // place says otherwise. A reader at the foot has no place, and the
      // mount's own foot pin lands them.
      const place = untrack(() => places.of(seat));
      if (place !== undefined) {
        restoring = place;
        restoreStarted = 0;
        footHeld = false;
        working?.following(false);
      }
      return;
    }
    restoring = null;
    anchor = null;
    placeKept = false;
    rememberPlace();
    footHeld = true;
    // **A successor's first pin is a first pin.** `placed` is the offset the
    // glide branch compares against, and the previous conversation's foot is
    // not this one's: leaving it set slides the new conversation in from the
    // old offset instead of landing at once (the book's "a first pin, a
    // switch's or a return's, lands at once"). The mount's clear is not
    // enough on its own - a page that lands before a render pass sees the
    // emptied record leaves the old list mounted - so the swap clears here
    // too, and the settle with it.
    placed = null;
    openPlaced = false;
    stopGlide();
    working?.following(true);
  });

  // A hold that outlived its column must not ask for a seat nothing draws.
  $effect(() => () => {
    entered += 1;
  });

  /**
   * The reader's place, kept per seat for a mount that outlives the column.
   *
   * **Written where the anchor moves, not from a publish.** The anchor is a
   * plain field and a reader who keeps scrolling while parked changes nothing
   * the fold publishes - `following(false)` on an already-off follow is the
   * same record - so a place written from an effect would stay at the first
   * nudge while the reader kept scrolling. A reader at the foot has no anchor
   * to hold, so a seat left at its foot has no place. A row of a conversation
   * this seat no longer holds is not this seat's place, so it is not written
   * over one.
   */
  function rememberPlace(): void {
    const here = anchor;
    if (here === null) {
      // **A give-up that keeps the place outlives its own landing.** The
      // reader is carried to the latest, whose pin echoes back through here
      // with no anchor; without this the landing deletes the very place the
      // give-up kept for the next return.
      if (placeKept) return;
      places.forget(seat);
      return;
    }
    placeKept = false;
    // The membership scan runs when the row changes, not on every pixel of a
    // scroll within the same one. Untracked, because a caller is an effect
    // whose pin must not re-run on the dependency this read would add.
    if (places.of(seat)?.anchor.key !== here.key) {
      const mine = untrack(() => held.turns.some((turn) => turnHasRow(turn, here.key)));
      if (!mine) return;
    }
    places.keep(
      seat,
      untrack(() => held.sessionId),
      here,
    );
  }

  /**
   * Take a returning reader back to the row their place names.
   *
   * **A fresh list mounts at its start**, so the row may be outside the drawn
   * window until the list is moved to its turn. A turn is the list's item, so
   * scrolling to it draws every row in it - and the ordinary anchor restore
   * then puts the reader exactly back. A restore that cannot land is given up
   * and the reader goes to the foot, rather than staying at the turn the
   * scroll reached with the follow off.
   */
  $effect(() => {
    const place = restoring;
    const handle = list;
    const turns = held.turns;
    if (place === null || handle === null || turns.length === 0 || restoreFrame !== null) return;
    // **A conversation whose occupant has moved on is not this place's.**
    // The place is taken before the id is known on a fresh connection, so the
    // id that arrives is what confirms or drops it; the same check covers a
    // `/resume` landing under a return.
    const session = held.sessionId;
    if (session !== null && place.session !== null && session !== place.session) {
      giveUpRestore();
      return;
    }
    if (turnFor(place.anchor.key) === undefined) {
      // **A row outside the loaded window is not a row the conversation has
      // lost.** `cursor` says older turns are still loadable, so ask for the
      // page above and wait for the landing that re-runs this effect: the
      // row is in it, and the reconnect that rebuilt the record is not the
      // conversation outliving the reader's place.
      if (held.cursor !== null && held.loaded) {
        if (restoreStarted === 0) restoreStarted = Date.now();
        if (Date.now() - restoreStarted < RESTORE_MS) {
          loadOlder(MORE_TURNS);
          return;
        }
      }
      // **A place the window merely has not reached is not deleted**: the
      // reader goes to the latest, and the stored place stays for a later
      // return. Only a conversation with nothing left above has outlived the
      // row - and only that one's place goes.
      giveUpRestore(held.cursor !== null);
      return;
    }
    restoreStarted = Date.now();
    restoreFrame = requestAnimationFrame(() => {
      // **The turn is read again in the frame it is scrolled in**: a prepend
      // can land between this schedule and its frame, and an index from the
      // earlier read would name a different turn.
      const at = held.turns.findIndex((turn) => turnHasRow(turn, place.anchor.key));
      if (at < 0) {
        giveUpRestore();
        return;
      }
      handle.scrollToIndex(at, { align: 'start' });
      restoreFrame = requestAnimationFrame(placeRestored);
    });
  });

  /** Put the reader back on the row once its turn is drawn, or give it up. */
  function placeRestored(): void {
    restoreFrame = null;
    const place = restoring;
    if (place === null) return;
    if (viewport === null || held.following) {
      giveUpRestore();
      return;
    }
    if (viewport.querySelector(`[data-k="${CSS.escape(place.anchor.key)}"]`) !== null) {
      restoring = null;
      restoreTries = 0;
      anchor = place.anchor;
      // **The place is taken in the frame its row is drawn.** A switch back
      // has no landing behind it to wake the parked pass, so waiting for the
      // next publish would leave the reader at the turn's start until one
      // came.
      restoreAnchor();
      return;
    }
    const turn = held.turns.find((one) => turnHasRow(one, place.anchor.key));
    /** Whether the row's TURN is drawn, whatever the row is doing. */
    const drawn =
      turn !== undefined && viewport.querySelector(`[data-t="${CSS.escape(turn.key)}"]`) !== null;
    // **The turn not being drawn is not the row being gone.** A huge live turn
    // is the list's one item: after a jump the list lands on its estimate,
    // measures it, and re-places it, so the turn can be outside the window for
    // more than a couple of frames. The row is not re-keyed or taken - the
    // window has not reached it - and giving up here drops the reader to the
    // foot for a merely slow mount. The list is asked again, within a budget;
    // past the budget the reader goes to the foot rather than waiting on a
    // mount that never comes.
    if (!drawn) {
      const handle = list;
      if (Date.now() - restoreStarted >= RESTORE_MS || turn === undefined || handle === null) {
        giveUpRestore();
        return;
      }
      const at = held.turns.findIndex((one) => one.key === turn.key);
      if (at >= 0) handle.scrollToIndex(at, { align: 'start' });
      restoreFrame = requestAnimationFrame(placeRestored);
      return;
    }
    // The turn is here and the row is not: the row was re-keyed or taken, and
    // the place goes the way the rules say - to the latest.
    restoreTries += 1;
    restoreFrame = restoreTries < RESTORE_TRIES ? requestAnimationFrame(placeRestored) : null;
    if (restoreFrame === null) giveUpRestore();
  }

  /**
   * The old entry when a restore cannot land: the reader goes to the foot,
   * following, rather than staying at the turn's start with the follow off.
   *
   * **`keepPlace` is the row the loaded window has not reached.** A place the
   * conversation still has history under is not one it has outlived: the
   * reader is carried to the latest, and the stored place stays for the next
   * return rather than being deleted by a page that merely had not been read
   * yet.
   */
  function giveUpRestore(keepPlace = false): void {
    restoring = null;
    restoreTries = 0;
    restoreStarted = 0;
    anchor = null;
    placeKept = keepPlace;
    if (!keepPlace) rememberPlace();
    footHeld = true;
    working?.following(true);
    land();
  }

  // A restore still waiting must not land on a column that is going.
  $effect(() => () => {
    if (restoreFrame !== null) cancelAnimationFrame(restoreFrame);
    restoreFrame = null;
    if (captureFrame !== null) cancelAnimationFrame(captureFrame);
    captureFrame = null;
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

  /**
   * Force the measure when a list mounted over turns drew no rows (#2830),
   * with room to draw them. One mount per open, so a list still empty after it
   * is not re-mounted on a loop.
   *
   * **A drawn row is the only healthy answer, not a reported viewport.**
   * `virtua` answers its last range while a scroll correction is parked, and a
   * correction the list cannot scroll to - the content already fits, or the
   * offset is clamped at an end - is never cleared. The range then stays the
   * pre-measure `[0, -1]` for the life of the mount while the viewport reads
   * healthy: on the 2026-11-04 open the record says 0 rows over a 1002px pane
   * (652.25 in the repro), and nothing drew until a seat switch remounted it.
   */
  function checkMeasured(): void {
    checkFrame = null;
    if (remeasured || held.turns.length === 0 || viewport === null) return;
    // **No room is no answer.** A hidden or collapsed pane has nothing to draw
    // yet, and the size that gives it room re-arms this check through the
    // watcher below.
    if (viewport.clientHeight <= 0) return;
    if (viewport.querySelectorAll('[data-t]').length > 0) return;
    remeasured = true;
    listEpoch += 1;
  }

  /**
   * Check two frames out: ResizeObserver notifications are delivered after a
   * frame's rAF callbacks, so the first frame would read a healthy list as
   * unmeasured in the same update that measures it.
   */
  function scheduleMeasureCheck(): void {
    if (remeasured || checkFrame !== null) return;
    checkFrame = requestAnimationFrame(() => {
      checkFrame = requestAnimationFrame(checkMeasured);
    });
  }

  /**
   * **Every list mount is checked for its own measure** (#2825): the effect is
   * keyed on the viewport the list binds, which changes exactly when a mount
   * does - not on the conversation, which a live seat replaces every frame.
   */
  $effect(() => {
    if (viewport === null) return;
    if (untrack(() => !held.loaded || held.turns.length === 0)) return;
    scheduleMeasureCheck();
  });

  /**
   * **An open that empties re-arms the one forced mount** (#2825) - an
   * occupant swap, a refusal, the seat going - so the next conversation this
   * column draws gets its own if the gate closes on it.
   */
  $effect(() => {
    if (!held.loaded || held.turns.length === 0) remeasured = false;
  });

  // A check still waiting must not land on a column that is going.
  $effect(() => () => {
    if (checkFrame !== null) cancelAnimationFrame(checkFrame);
    checkFrame = null;
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
  /** The report unit the pin holds, so its detach into the turn is known. */
  let carriedRow: string | null = null;
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

  /**
   * Let the pin's row go, recording it as arrived.
   *
   * **The pin draws outside the conversation**, so the row it has been holding
   * was never observed; when it detaches into the turn the reader sees it
   * there for the first time, and that is a move rather than an arrival.
   */
  function releasePin(): void {
    // Read untracked: this runs from the pin's own effect, and a tracked read
    // of `carried` would make the effect's own write a reason to run again.
    const turn = untrack(() => carried);
    if (turn === null) return;
    // **The row the pin holds NOW, which a second result inside the beat can
    // have changed**, and the one it held when the beat began: both draw in
    // the turn when the pin lets go.
    const holding = untrack(() => (carried === newest && pinned !== null ? pinned.key : null));
    for (const row of new Set([carriedRow, holding])) {
      if (row !== null) recordArrival(turn, `${turn}:${row}`);
    }
    carriedRow = null;
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
        releasePin();
        carried = which;
        beating = false;
        stopBeat();
      }
      carriedRow = newestRow.key;
      return;
    }
    // A row that arrived finished is the turn's own, not the pin's: only the
    // turn this column watched run is carried over, and only once.
    if (untrack(() => carried) !== which || untrack(() => beating)) return;
    carried = which;
    carriedRow = newestRow.key;
    beating = true;
    stopBeat();
    beatTimer = setTimeout(() => {
      releasePin();
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
  function land(force = false): number {
    if (viewport === null) return 0;
    // **The pin may move the reader only where they stand at the foot, or
    // where their own send armed the follow.** Every other arm of the follow -
    // a handoff's successor, a turn landing, a new row - is an arrival, and an
    // arrival holds the position the reader chose.
    // Read untracked: the pass runs from effects, and its own write to
    // `footHeld` must not make that effect a reason to run again - a second
    // pass would write the same offset and record a pin nobody asked for.
    if (!force && !untrack(() => pinning())) return 0;
    // **The reader's own ask is the arm, and it outlives a held-back pass.**
    // A forced pass can still be stopped below by a reserved row with nothing
    // written; the next pass is unforced, so unless the ask counts as standing
    // at the foot here, the click that hid its own control is never paid.
    if (force) footHeld = true;
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
    // **An open PLACES its reader; only a settled following column holds.**
    // The hold stops the pin at the end the list has measured, so a row that
    // grew in this update is not scrolled past before the measurement lands.
    // That is the right answer while a turn is live and the reader is
    // following it. It is the wrong answer on an open: a kept conversation's
    // rows are already in the element while the list measures them a pass
    // later, so the model's foot sits far ABOVE the element's - and holding
    // there lands the reader near the top of the conversation and then walks
    // them down as the measurement catches up. `openPlaced` is false until
    // the model reaches the element's own end, so it is the open placing its
    // reader: write the element's own foot and do not schedule the retries a
    // hold would.
    if (model !== null && max > model && placed !== null && openPlaced) {
      // **The pin never passes the end the list has measured.** It holds
      // there and comes back for the growth on a later frame; the two-frame
      // budget bounds only the deferred retries, never the hold, so a stream
      // that grows the row every frame still cannot write past the model.
      // **And it never drags the reader back to that end.** The hold is a
      // ceiling on a forward write; a reader already past the model's foot -
      // an entry that placed at the element's, with rows arriving below it -
      // would otherwise be pulled up to the model and glided back down as it
      // measured, which is the entry walk this column is meant to have lost.
      target = Math.max(model, viewport.scrollTop);
      if (repinTries < REPIN_TRIES) {
        repinTries += 1;
        repin();
      }
    } else {
      repinTries = 0;
    }
    // The furthest end this pin may reach, in the element's own offsets.
    const reachable = model !== null && max > model ? model : max;
    // **A following column glides to the foot; a jump is the reader's own.**
    // The button's press (`force`) and a reader who asked for less motion
    // take the same number in one write - both ARE the landing, and there is
    // nothing to watch move. **So does an open**: the first pin of an open,
    // a switch's or a return's, places the reader rather than following a
    // foot that moved, and `placed` is null until one has landed. Every later
    // pass travels there, so a streamed line and a row measuring taller read
    // as the column following rather than as the page snapping.
    // **A pass that has nowhere to travel takes the write below**, which is
    // one offset already at its end rather than 160ms of frames writing it.
    const want = Math.min(target, max);
    if (
      !force &&
      placed !== null &&
      openPlaced &&
      wantsMotion() &&
      Math.abs(want - viewport.scrollTop) > FOOT_QUANTUM
    ) {
      startGlide(want, reachable);
      return foot;
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
    // **Where it landed, and whether that is the foot.** The hold branch
    // writes short of the maximum on purpose, so the record cannot assume a
    // pin lands at the foot; the read-back already says which it was.
    if (viewport.scrollTop !== was) {
      pinEcho = { at: viewport.scrollTop, foot: viewport.scrollTop >= max };
    }
    // Where the column last left the reader, which is the foot the browser
    // clamped the pin to.
    placed = viewport.scrollTop;
    // **The mount has settled when the model reaches the element's own end.**
    // Until then every pass is the entry placing its reader - instantly - and
    // only the first growth after it is the column following.
    if (!openPlaced && (model === null || max <= model)) openPlaced = true;
    // **The pin leaves the reader at the end it could reach**: the browser's,
    // or the list's own while the browser counts a row it has not drawn. Both
    // are the foot for the next arrival's decision - give or take the fraction
    // the browser rounds the offset to.
    // **The value is read here rather than back out of `footHeld`**: `land()`
    // runs from the follow's own effect, and a read of that state would make
    // the write below a dependency - the effect re-runs and pins twice for one
    // landing.
    const landedAtFoot = viewport.scrollTop >= reachable - FOOT_QUANTUM;
    footHeld = landedAtFoot;
    // **A pin that lands at the foot is the reader leaving their place.** The
    // pin's own echo returns to the scrolled handler before the foot branch
    // there, so the record has to be cleared here - a place left standing is a
    // row a later document return restores for a reader who moved on.
    if (landedAtFoot) {
      // **A send is the reader asking for the latest**, so it gives up a place
      // a restore gave up on and kept. A landing nobody asked for - the
      // give-up's own pin, an append's - leaves it for the next return.
      if (held.send_armed) placeKept = false;
      anchor = null;
      rememberPlace();
    }
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
    // **A restore in flight owns the placement.** A return re-arms the hold
    // while `restoring` still names the row to place; a follow pass there
    // lands the short element's foot, writes the offset with no anchor and
    // deletes the place the reader left. The restore's own frame places them.
    if (restoring !== null || follows === null || !held.loaded || !held.following) return;
    land();
    // A landing while the reader is at the latest can take the held set over
    // the window, and this is the place that knows what is on screen: the
    // window settles here rather than in the conversation, which cannot see
    // whether the column fits.
    settleWindow();
  });

  /**
   * The badge: every turn appended since the reader parked, capped at nine.
   *
   * The mark is the turn the reader parked after, taken on the first parked
   * run. A reader who returns to the foot is following again, so the mark
   * goes and the next park starts from the turn that is newest then.
   */
  $effect(() => {
    const turns = held.turns;
    if (!parked) {
      badgeFrom = null;
      badge = 0;
      return;
    }
    if (badgeFrom === null) {
      badgeFrom = turns[turns.length - 1]?.key ?? null;
      badge = 0;
      return;
    }
    const at = turns.findIndex((turn) => turn.key === badgeFrom);
    if (at < 0) {
      // The turn the reader parked after is no longer held, so the stretch
      // they missed cannot be counted from it; the next park starts fresh.
      // **A seat switch re-arms by resetting the marker**, so this branch is
      // only a marker the record itself lost (a merge or a compaction) - and
      // that one restarts the count rather than guessing at it.
      badgeFrom = null;
      badge = 0;
      return;
    }
    badge = turns.length - 1 - at;
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
    // **A return's own placer owns the scroll until its row is drawn.** The
    // pass runs again when the place is taken or dropped, so nothing is owed
    // for the frames it stands aside on.
    if (restoring !== null) return;
    // **A conversation that no longer holds the anchored row goes to the
    // latest** (the rule a compaction or a trim is written for). The pass
    // below would retry a lookup for a row nothing holds, and a parked reader
    // would sit on a place that no longer exists; the latest is the one place
    // that does. Not a clamp to whatever row is left - that is a place the
    // reader never chose. **Only over a conversation that has loaded**: a
    // replacement empties the record until its successor's page arrives, and
    // that transient is not rows gone - a re-spawn onto the same claude
    // session must keep the place it is about to draw again.
    const heldBy = anchor;
    if (
      park.loaded &&
      heldBy !== null &&
      !park.turns.some((turn) => turnHasRow(turn, heldBy.key))
    ) {
      // **A row the loaded conversation no longer holds goes to the latest.**
      // The trim and the compaction drop whole pages, and a place naming a
      // row nothing draws is a place the pass would retry forever. **A row a
      // window has merely not read yet is the restore's**: it owns the
      // placement while it pages toward the turn (`restoring`), so a parked
      // pass here is looking at a record that has already outlived the row.
      anchor = null;
      placeKept = false;
      rememberPlace();
      footHeld = true;
      working?.following(true);
      land();
      return;
    }
    // **A follow that cannot pin is a parked reader here too.** The gate can
    // refuse a follow that is on (the third state the way-back and the
    // observer's restore already serve), and skipping the pass on the flag
    // alone would leave that reader with neither the pin nor the anchor.
    if (anchor === null || (park.following && pinning()) || !park.loaded || moving) {
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
    // **A return's own placer owns the scroll until its row is drawn**, the
    // same reason the hold pass stands aside. The mount's own events are not
    // the reader's: a fresh list reports its start at zero, and a returning
    // one reports its pre-place position, which can read as the foot and
    // re-arm the follow over the place being restored. The tracker still
    // moves, so the reader's first gesture after the landing reads its own
    // direction.
    if (restoring !== null) {
      // **A move the return owns is still written.** Its placement is a
      // machine move like any other, and "every move is written" is how a
      // reader of the log tells a jump from a scroll - skipping these frames
      // made a return the one stretch with no record at all.
      noteScrollMove({
        from: movedFrom,
        to: offset,
        height: viewport?.scrollHeight ?? 0,
        room: viewport?.clientHeight ?? 0,
        shrank: false,
        rose: false,
        foot: atFoot(),
        reader: false,
        aimed: false,
        restore_owed: true,
        pin: false,
      });
      movedFrom = offset;
      return;
    }
    // The event's own direction, read before any branch moves on: a scroll
    // that came down toward the foot is the reader's return, and it arms no
    // up-capable wheel token at all - Ved's own gesture is "scroll up and
    // come back down" (2026-10-11). Every event updates the tracker, the
    // pin's echo included: a pin is a write like any other move.
    const towardFoot = offset > movedFrom;
    const was = movedFrom;
    movedFrom = offset;
    // **The column's own glide is not the reader moving.** Every frame of it
    // comes back as a scroll event, and reading those as a hand would disarm
    // the follow partway through the landing the follow asked for. The last
    // frame's completion plants the pin token, so that event is the echo it
    // already knows how to read.
    if (glideStop !== null) {
      // **A write the glide did not make is someone else's move.** The
      // reader's own gesture is heard separately - `arm` and `aim` stop the
      // glide before their own event arrives - so a foreign write here is a
      // machine move: the list's own correction, a restore, a click on a
      // dispatch. **It is still the column's landing, though**: the reader
      // asked to come back, and a layout correction half way through does not
      // take that back. The glide stops, the move's offset stands, and a
      // column still standing to land takes the foot again from there - the
      // alternative strands a reader mid-travel with the follow switched off
      // by a correction they never made.
      if (Math.abs(offset - glideAt) > FOOT_QUANTUM) {
        stopGlide();
        if (pinning()) land();
        lastSeen = {
          content: viewport?.scrollHeight ?? 0,
          room: viewport?.clientHeight ?? 0,
        };
        return;
      }
      lastSeen = {
        content: viewport?.scrollHeight ?? 0,
        room: viewport?.clientHeight ?? 0,
      };
      return;
    }
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
      if (offset === written.at) {
        // **The pin is the machine move a reader most often feels**, so it is
        // written before the echo's own early return. The last event's heights
        // are reused rather than the layout read again - the per-frame cost
        // #1710 measured is exactly this path's - and a pin lands at the foot
        // by construction.
        noteScrollMove({
          from: was,
          to: offset,
          height: lastSeen.content,
          room: lastSeen.room,
          shrank: false,
          rose: false,
          foot: written.foot,
          reader: false,
          aimed: false,
          restore_owed: owed !== null,
          pin: true,
        });
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
    /** Whether the reader stood at the foot before this event moved them. */
    const wasFoot = footHeld;
    // **Where the reader is now, for the pin's next decision.** Only a move
    // moves it: a size change moves the foot under a reader standing still,
    // and the follow is what absorbs that. A few pixels above the end is the
    // notch a row's measurement leaves, not a place of their own.
    if (offset !== was) footHeld = height - offset - room <= FOOT_NEAR;
    // **Every move is written, flagged.** A machine move has no other record -
    // the draw says where the list stands and never that the reader was
    // carried - and the flags are what separate it from the reader's own: a
    // filter for `reader: false && aimed: false` is the one worth reading.
    noteScrollMove({
      from: was,
      to: offset,
      height,
      room,
      shrank,
      rose,
      foot,
      reader: byReader,
      aimed,
      restore_owed: owed !== null,
      pin: false,
    });
    // **A layout move is not the reader leaving.** The browser's own scroll
    // anchoring shifts the offset toward the foot when rows above measure; a
    // following column it leaves short of the foot goes back rather than
    // adopt the displacement. Only that shape: the reader stood at the foot
    // (`wasFoot`), the follow is on, and no hand made the move. `!footHeld`
    // is this event's landing: a landing inside the notch already counts as
    // holding the foot, so a re-land there would write the exact foot and
    // move a reader the column has just treated as standing at it.
    // No shrink term: a net shrink with an anchoring shift can still leave a
    // following column short - a pin held at the model end does - and landing
    // the foot again is the right recovery there.
    // `restoring` is a return still placing its reader, not a foot to land.
    if (
      wasFoot &&
      held.following &&
      towardFoot &&
      !footHeld &&
      !byReader &&
      !aimed &&
      restoring === null
    ) {
      land(true);
      return;
    }
    if (foot) {
      // **A return that lands at the foot re-arms, whatever the size did.**
      // (The reasons below are the exact-foot arm's; the notch arm that
      // follows shares them.)
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
        rememberPlace();
      }
    } else if (
      // **The notch resumes the follow, not only the exact end.** A reader who
      // comes back within `FOOT_NEAR` of the bottom is at the latest as far as
      // the column's own gate is concerned (`footHeld` above), and leaving the
      // follow off there strands them: the next append could not land and the
      // badge would keep counting over a reader who is all but at the foot.
      // **The move has to be the READER'S**: a hand heard, or a gesture
      // aiming at the foot. A machine move that happens to land there - the
      // pin's own echo, a layout correction - is the column's, and re-arming
      // on it would turn a reader away from the foot back into a follower.
      // **And it has to be a move**: a fresh list reports its own start as an
      // event at zero, which is neither a reader arriving nor a foot.
      offset !== was &&
      footHeld &&
      !held.following &&
      (byReader || aimed)
    ) {
      working?.following(true);
      settleWindow();
      anchor = null;
      rememberPlace();
      // The place was just given up, so the capture below must not take a new
      // one from this event's own offset.
      return;
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
    if (!foot) {
      if (captureAnchor()) owed = null;
      // **A jump can outrun the list's window.** The capture reads the rows
      // the last window left drawn, finds none under the edge, and asks again
      // once the list has re-windowed.
      else retryCapture();
    }
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
  <!-- A key a forced re-measure moves (#2825): the list has no measure call,
       so a fresh mount is the measurement, and the mount is what re-registers
       the ResizeObserver that failed to report. -->
  {#key listEpoch}
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
          {@attach (node: HTMLElement) => measureTurn(node, turn)}
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
  {/key}
  <!-- An older page in flight, said at the column's top edge: the wheel is
       clamped at the top for as long as the ask takes, and with nothing drawn
       the column reads as a dead scroll rather than as one waiting on a
       page (#2398). It stands over the rows and takes no pointer. -->
  {#if outstanding > 0}
    <div class="older-loading" role="status">Loading older messages…</div>
  {/if}
  <!-- The way back to the foot, shown whenever the column is not pinning:
       a reader parked with the follow still on has no other door, and the
       click is the whole way back - at the foot and following again, in one
       move. -->
  {#if parked}
    <button
      class="follow"
      type="button"
      title="back to the latest"
      aria-label={badge === 0
        ? 'back to the latest'
        : `back to the latest, ${badge >= 9 ? '9 or more' : badge} new ${
            badge === 1 ? 'turn' : 'turns'
          }`}
      onclick={() => {
        // The press is a jump, so the finish the column was gliding to is not
        // the reader's: the write below takes the viewport at once.
        stopGlide();
        working?.following(true);
        settleWindow();
        anchor = null;
        rememberPlace();
        // The reader asked for the foot, so this pass is theirs to make.
        land(true);
      }}
    >
      <Icon name="down" />
      {#if badge > 0}
        <span class="n" aria-hidden="true">{badge >= 9 ? '9+' : badge}</span>
      {/if}
    </button>
  {/if}
{/if}
<!-- The waiting prompts stand in EVERY conversation state, not only under the
     list: a seat coming up, refused, not yet read or empty holds the same tree,
     tasks and watchers as one mid-turn. The strip and the box are one surface
     now, drawn by the composer - the column publishes its pinned row through
     `pinnedRow` and nothing else stands at this foot. -->
{#if queue !== undefined}{@render queue()}{/if}

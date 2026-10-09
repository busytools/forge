<script lang="ts">
  import { untrack, type Snippet } from 'svelte';
  import { SvelteMap } from 'svelte/reactivity';
  import { VList, type VListHandle } from 'virtua/svelte';
  import { ROW_ESTIMATE, sizes } from './row-sizes';

  import Icon from '../components/Icon.svelte';
  import { subjectKey } from '../protocol';
  import { scrollAsk } from '../session/scroll-ask';
  import type { Connection } from '../socket';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import { anchoredScroll, anchorAt, type Anchor, type RowBox } from './anchor';
  import { latestCompaction } from './compaction-jump';
  import {
    Chat,
    NOTHING,
    beingWritten,
    runningAt,
    type Conversation,
    type Turn as HeldTurn,
  } from './conversation';
  import Echo from './Echo.svelte';
  import { echoes, ownWords } from './echoes.svelte';
  import Pinned from './Pinned.svelte';
  import Turn from './Turn.svelte';
  import { turnOfDispatch } from './dispatch-jump';
  import { callRow, reachableIds, reveal, subagents } from './subagents.svelte';
  import { fold, type TurnInfo } from './units';

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
    reason = null,
    queue = undefined,
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The roster holds no session for this seat, which is its own state. */
    waking?: boolean;
    /** Whether the core is bringing the seat up, the waking line's other half. */
    spawning?: boolean;
    /** Why, when it does. */
    reason?: string | null;
    /** The waiting prompts, drawn above the pinned row: the page owns the
     *  queue's data, and its place is the column's foot. */
    queue?: Snippet | undefined;
  } = $props();

  /** How near the top the reader has to be before the turns above are asked for. */
  const REACH = 400;
  /** How long the arrival mark stays on the newest item's wrapper. */
  const ARRIVE_MS = 300;
  /** How far above the last pin counts as the reader when no input preceded it. */
  const DISARM_SLACK = 48;
  /** How long after a wheel, touch or up-scrolling key its events read as the reader's. */
  const READER_WINDOW_MS = 250;
  /** The keys a focused row scrolls the column UP with, which are the ones that may disarm. */
  const SCROLL_UP_KEYS = new Set(['ArrowUp', 'PageUp', 'Home']);

  /**
   * The conversation, held as a VALUE: a frame replaces the record rather than
   * changing it, so nothing in it needs tracking - deep state made every read
   * of every message a proxy call on each frame's re-derivation.
   */
  let held = $state.raw<Conversation>(NOTHING);
  /**
   * The element that scrolls, which is the list's own viewport.
   *
   * Held so the follow can pin the foot by asking the BROWSER where it is:
   * the list's reported size is a measurement that lags a row that grew in
   * this update, and the element's own `scrollHeight` does not.
   */
  let viewport: HTMLElement | null = $state(null);
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
  /**
   * The row the reader's own eye is on, held while they are away from the foot.
   *
   * **Measured in rows rather than pixels** (the terminal's rule, and
   * `anchor.ts` carries the reasoning): the layout moves under a reader who is
   * not at the foot - a row measures taller once drawn, a stretch is corrected
   * as frames land - and an offset that stays put reads as the page sliding
   * under them (Ved, 2026-10-03). It goes the moment the follow is back on,
   * because the foot is where that reader wants to be.
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
  /** The newest turn's key: the row the carried beat and the reader's echo ride. */
  const newest = $derived(newestTurn?.key ?? null);
  /**
   * The item whose arrival fade is owed, dropped again after the window.
   *
   * The mark cannot sit on `newest` itself: a CSS animation restarts per
   * element insertion, and the list recreates items as they leave its
   * window - a class still carried would replay the fade on the next scroll
   * back to the foot. The window is twice the animation's length.
   */
  let arriving = $state<string | null>(null);
  $effect(() => {
    const key = newest;
    if (key === null) return;
    arriving = key;
    const timer = setTimeout(() => {
      if (arriving === key) arriving = null;
    }, ARRIVE_MS);
    return () => clearTimeout(timer);
  });

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
    echoes.post(seat, held.words, runningAt(connection, slot, newestTurn?.running === true), id);
    try {
      // Fire-and-forget like the composer's own send: the outcome rides the
      // subscription rather than a reply, so there is nothing to await.
      // No seat handed over: this retry's refusal is drawn by the echo row
      // below, with the retry button the reader came here for - a notice line
      // beside it would tell the same loss twice.
      void connection.dispatch({
        prompt_under: { key: slot, text: held.words, attachments: [], uuid: id, source: 'you' },
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
   * Every seat's conversation, kept after the reader leaves it.
   *
   * A switch used to throw the current one away and ask the server again, so a
   * seat already drawn came back as a blank column and a round trip. The record
   * for every seat this client has visited is kept for the same reason
   * (`session/live.ts`); this is the half that was missing.
   *
   * **Nothing draws from either map, and every read of them is untracked**: the
   * seat on screen is `held` above, and a tracked read here would make the
   * effect below depend on the entry the subscription writes on every frame,
   * re-running it per frame with nothing about the seat changed.
   */
  const kept = new SvelteMap<string, Conversation>();
  /** The seats whose conversation is still open here, so a switch back does not open a second. */
  const live = new SvelteMap<string, { connection: Connection; chat: Chat; stop: () => void }>();
  /**
   * Pages of older turns asked for and not yet answered.
   *
   * **Counted rather than flagged, and armed by the ASK.** The compensation
   * has to be on while a prepend lands and off for everything else, and both
   * halves of that are edges: an ask the socket refused must not arm it at
   * all, and a second ask still in flight must not be disarmed by the first
   * page landing. One flag cannot tell those apart from the ordinary case.
   */
  let outstanding = $state(0);
  /** Whether the last prepend has landed but the list has not taken it yet. */
  let settling = $state(false);
  /** The prepend count this component has already accounted for. */
  let accounted = 0;
  /** The dropped-ask count this component has already drained against. */
  let drained = 0;
  /** The content height at the last scroll event, which tells a reader moving from a layout moving. */
  let shaped = 0;
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
  /** The tick a settling compensation waits on, held so a later one can replace it. */
  let timer: ReturnType<typeof setTimeout> | null = null;

  const shift = $derived(outstanding > 0 || settling);
  /** This column's own memory of its rows' heights, kept out of a module global. */
  const rowSizes = sizes();
  // The measured heights belong to the turns this conversation holds.
  $effect(() => rowSizes.prune(held.turns.map((turn) => turn.key)));

  /** Whether the events arriving are the reader's own, made moments ago. */
  function readerMoved(): boolean {
    const seen = readerAt;
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
   * The same event is the terminal's other re-arm: its clamp re-engages the
   * follow whenever the content fits under the reader - a window made large
   * enough, a page that shrank - and a reader parked at what is now the very
   * end is at the end, whether or not anything scrolled to say so.
   */
  /**
   * Record what a turn's row measures, so a row the send's pull-and-return
   * keeps mounted holds its own space while it draws nothing (#1890).
   */
  function measureTurn(node: HTMLElement, key: string): () => void {
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) rowSizes.remember(key, entry.contentRect.height);
    });
    observer.observe(node);
    return () => observer.disconnect();
  }

  function scrollViewport(node: HTMLElement): () => void {
    viewport = node;
    // A token armed against a previous viewport must not eat this one's first event.
    pinEcho = null;
    // The reader's own input, and the only evidence of a hand the column has:
    // scroll events carry no source, so the gesture has to be heard separately
    // - and only the UP-capable kinds, because a gesture that cannot move the
    // reader up cannot have moved them up (the terminal disarms from
    // `scroll_up` alone). A touch arms whole: it carries no direction.
    const arm = () => {
      readerAt = performance.now();
    };
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) arm();
    };
    const onKey = (event: KeyboardEvent) => {
      if (SCROLL_UP_KEYS.has(event.key) || (event.key === ' ' && event.shiftKey)) arm();
    };
    node.addEventListener('wheel', onWheel, { passive: true });
    node.addEventListener('touchstart', arm, { passive: true });
    node.addEventListener('touchmove', arm, { passive: true });
    node.addEventListener('keydown', onKey);
    const content = node.firstElementChild;
    const watcher = new ResizeObserver(() => {
      if (holdsEverything()) {
        working?.following(true);
        anchor = null;
      }
      if (held.following) land();
      // A reader away from the foot has a place of their own, and a size change
      // is one of the two ways it moves under them. **Not while the prepend
      // compensation is on**, same as the effect's own pass: that path holds
      // the reader by the list's own shift as older turns arrive above them,
      // and two hands on the scroll is one too many.
      else if (!shift) {
        restored += 1;
        restoreAnchor();
      }
    });
    watcher.observe(node);
    if (content !== null) watcher.observe(content);
    return () => {
      watcher.disconnect();
      node.removeEventListener('wheel', onWheel);
      node.removeEventListener('touchstart', arm);
      node.removeEventListener('touchmove', arm);
      node.removeEventListener('keydown', onKey);
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

  /** Hold the row the reader's top edge is on, which is what their place means. */
  function captureAnchor(): void {
    if (viewport === null) return;
    const landed = anchorAt(drawnRows(), viewport.getBoundingClientRect().top);
    if (landed !== null) {
      anchor = landed;
      // A fresh capture owes nothing until an order moves under it; the next
      // parked run schedules against a clean slate.
      owed = null;
    }
  }

  /**
   * Put the reader back on the row the anchor holds, wherever the layout moved
   * it to - and nowhere at all when the row is out of the drawn window, since
   * the column cannot measure where it went. The anchor stays for a pass that
   * can, and the row coming back into the window is a size change like any
   * other.
   *
   * **The key is the turn's own plus the unit's**, which `Turn` writes, so the
   * lookup cannot land on a row of another turn: the fold names an id-less
   * frame `f<index>` within its own turn, and two turns can each carry one.
   */
  function restoreAnchor(): void {
    const held = anchor;
    if (held === null || viewport === null) return;
    const row = viewport.querySelector(`[data-k="${CSS.escape(held.key)}"]`);
    if (row === null) return;
    const box = viewport.getBoundingClientRect();
    const top = row.getBoundingClientRect().top - box.top + viewport.scrollTop;
    const want = anchoredScroll(held, top);
    if (Math.abs(want - viewport.scrollTop) >= 1) viewport.scrollTop = want;
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

  $effect(() => {
    const which = seat;
    const open = connection;
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
    // every frame it receives wherever the seat is (and holds the seat's own
    // subscription - Ved, 2026-10-09), so the state never falls behind;
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
    if (fresh || which !== placedFor) {
      placed = null;
      // The place a reader held was a row of the conversation that is going.
      anchor = null;
      // **And so is the follow, which is why the entry re-arms it** (#1673):
      // a seat's conversation is kept, and its follow flag was kept with it -
      // so a seat left scrolled up came back with the pass returning early
      // and the reader landing wherever the old offset fell. A place belongs
      // to the visit; every entry lands at the latest.
      working?.following(true);
      // **And so were the asks this column is holding.** They belong to the
      // conversation that is going, and their pages are not coming here: left
      // standing they hold `shift` on, which is the guard that keeps the
      // restore out of a prepend's way and would keep it out for good.
      outstanding = 0;
      settling = false;
    }
    placedFor = which;
    // The seat coming on screen is put there from what was kept, not from a
    // read, which is the whole point of holding it.
    held = untrack(() => kept.get(which)) ?? NOTHING;
    // Leaving the seat: its store notifications stop with its frames.
    return () => {
      unsubscribe();
    };
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
      untrack(() => live.get(previous))?.chat.leaving();
    }
  });

  // The column's own teardown, which the effect above cannot do: it closes one
  // conversation only to open the next in its place.
  $effect(() => () => {
    for (const entry of live.values()) entry.stop();
    live.clear();
    kept.clear();
  });

  // The first page is drawn at the end rather than the start, and that is not
  // a special case: the column OPENS following, so the first page is pinned by
  // the follow below like any other. A reader who scrolls away from the end
  // turns the follow off and the column stays where they left it.

  // A page landing settles one ask, and the last one leaves the compensation on
  // for one more tick: `virtua` applies it as the rows change, so a page that
  // dropped it in the same update would be disarming before the change it was
  // armed for.
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
  // with none. Without this the count outlives the ask it was armed for, and
  // `shift` - the guard that keeps the restore out of a prepend's way - stays
  // on for the life of the seat, which is the parked reader's hold quietly
  // turning itself off (measured: the offset left at 50 where the row above the
  // reader had moved it to 250, still 50 after the reconnect's own page).
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
   * What the follow pass runs on: the seat and the turn count.
   *
   * **The seat travels in the key as consistency, not as the mechanism**:
   * what re-runs the pass on a switch is the arriving conversation's own
   * record being published to the column, which the effect watches (measured:
   * six constructions tried, none where the key decides) - so the seat is in
   * the key so two seats with the same number of turns cannot collide, belt
   * and braces beside the publish that does the work (#1673).
   */
  const follows = $derived(held.turns.length === 0 ? null : `${seat}:${held.turns.length}`);

  /**
   * The newest turn's own report row - the unit, and the figures in it -
   * folded the way the turn folds itself.
   *
   * **One fold and one pick, because the row has two readers**: the pin draws
   * it, and the turn's own copy of it stands aside. Which row that is is
   * answered here rather than worked out again at each of them.
   */
  const newestRow = $derived.by((): { key: string; info: TurnInfo } | null => {
    const turn = newestTurn;
    if (turn === null) return null;
    const units = fold(turn.messages, slot, beingWritten(turn), !beingWritten(turn));
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
   * Every message the page holds, across every turn.
   *
   * **A dispatch's row reads its instance's frames from here, not from its own
   * turn.** A backgrounded instance runs on past the turn that dispatched it,
   * so its calls arrive in LATER turns - looked for in the dispatch row's own
   * turn alone, a backgrounded instance's timeline reads empty.
   */
  const history = $derived(held.turns.flatMap((one) => one.messages));

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
   * Pin the foot: the scroll's own maximum, which is where the browser clamps.
   *
   * **Through the element rather than through the list's handle, and that is
   * what makes it land.** `scrollToIndex` computes its target from the row
   * sizes the list has already measured, and a row that grew in this very
   * update is measured a moment LATER - so a scroll computed that way lands at
   * the end of the previous total and leaves the newest content below the fold
   * until the next frame happens to arrive, which on a turn that has just
   * settled is never. Asking the element for `scrollHeight` cannot be early or
   * late: the browser clamps it to the foot of what is actually drawn.
   *
   * The terminal pins the same way: while `auto_scroll` is on, every render
   * sets its scroll target to its own max.
   */
  function land(): number {
    if (viewport === null) return 0;
    const foot = viewport.scrollHeight;
    // **The write is unconditional, and the token comes from the read-back.**
    // `scrollTop` clamps to `scrollHeight - clientHeight`, so what the write
    // lands on is not what it asked for, and only the read-back knows whether
    // the offset moved - a write of the value the element already holds is
    // not a scroll and fires no event, so arming the token for it would leave
    // it armed with no echo coming, to be eaten by the next real event the
    // browser fires - a clamp the follow then misreads as the reader.
    const was = viewport.scrollTop;
    viewport.scrollTop = foot;
    if (viewport.scrollTop !== was) pinEcho = viewport.scrollTop;
    // Where the column last left the reader, which is the foot the browser
    // clamped the pin to.
    placed = viewport.scrollTop;
    return foot;
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
            captureAnchor();
          }
        };
        const chase = () => {
          // The reveal scrolled: re-capture the anchor HERE, so the column's
          // own restorer (which runs on the layout mutations the reveal's
          // open causes) holds this new place rather than pulling the reader
          // back to the row they left.
          if (reveal(wanted)) {
            captureAnchor();
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
        // top is what made an older dispatch's click do nothing at all.
        loadOlder();
      }
      return;
    }
    const at = latestCompaction(held.turns);
    if (at !== null) {
      asking = false;
      list?.scrollToIndex(at, { align: 'start' });
      return;
    }
    if (!loadOlder()) asking = false;
  });

  // A reader at the end FOLLOWS the newest turn: that is what the end of a
  // conversation means, and a page that grew without the view moving would
  // lose the very thing it was opened on. A reader anywhere else is left
  // exactly where they are - the state is the reader's own, and the two
  // transitions that set it are the two below.
  $effect(() => {
    if (follows === null || !held.loaded || !held.following) return;
    land();
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
   * reader by the list's own shift as older turns arrive above them, and two
   * hands on the scroll is one too many.
   */
  $effect(() => {
    const park = held;
    const moving = shift;
    if (anchor === null || park.following || !park.loaded || moving) {
      // No anchor parked, nothing owed: the next parked run schedules against
      // a clean slate rather than against an order from before the episode.
      owed = null;
      return;
    }
    const above = keysAbove();
    if (above === null || above === owed) return;
    const want = above;
    const seen = restored;
    const settled = requestAnimationFrame(() => {
      // The debt is paid HERE, by the pass that runs - so a teardown's cancel
      // before the paint leaves it standing for the next run to re-arm.
      owed = want;
      if (restored === seen) restoreAnchor();
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

  /** Where the reader is, and whether they have reached the top. */
  function scrolled(offset: number): void {
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
        // never ask for what is above it.
        if (offset < REACH) loadOlder();
        return;
      }
    }
    // **The very end, with no reading threshold.** A reader a few pixels short
    // of it is mid-line, and a tolerance here is a column that moves under them
    // - the terminal's clamp re-engages its follow only at `scroll_offset >=
    // max_scroll` for exactly this reason.
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
    // scrolled away was carried back by whatever arrived. The height the
    // content had at the last event is what tells the two apart: an event whose
    // height has moved is the layout, and only an event at the height the
    // reader last saw can be them arriving at the end of it.
    // **Only a SHRINK can be a clamp.** Content arriving grows the column and
    // leaves the reader exactly where they are, so an event after one is the
    // reader's own; content corrected shorter takes the room out from under
    // them, and the event that follows is the browser moving them rather than
    // them moving. The one test covers the disarm below as well as the arming,
    // because a clamp that put the follow back on and a clamp that took it off
    // are the same mistake pointing two ways.
    const height = viewport?.scrollHeight ?? 0;
    const shrank = height < shaped;
    shaped = height;
    const foot = atFoot();
    if (!shrank) {
      if (foot) {
        working?.following(true);
        // The foot is where a following reader wants to be, so the place they
        // held on the way there is done with.
        anchor = null;
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
      else {
        const slack = readerMoved() ? 0 : DISARM_SLACK;
        if (offset < (placed ?? Infinity) - slack) working?.following(false);
      }
    }
    // **A reader away from the foot has a place, and this is where it is read.**
    // Their own scroll is the one moment the page is where they put it, so the
    // row under their top edge is what the column holds their place by from
    // here on. Read off this event's own arithmetic rather than the record:
    // the record's follow flag is the fold's, this disarm publishes at once,
    // and a STREAM fold that engages the follow - a prompt frame - lands a
    // painted frame later - so a capture that read the record would depend on
    // which side of that timing it caught.
    if (!foot) captureAnchor();
    if (offset < REACH) loadOlder();
  }

  /**
   * Ask for the turns above, holding the reader's place while they arrive.
   *
   * The compensation is armed as the ask goes out rather than when the answer
   * lands, because the list applies it AS the rows change: a page that waited
   * for the answer would be arming after the change it exists for, which is a
   * prepend that moves everything the reader is looking at.
   *
   * **And only when an ask actually went.** A socket that is down refuses the
   * ask without sending anything, so arming on the call rather than on its
   * answer leaves the compensation on with nothing coming - and the next turn
   * appended below the reader then goes through the prepend path, which moves
   * them AND leaves the list's measured sizes attributed to the wrong rows.
   */
  function loadOlder(): boolean {
    if (working?.older() !== true) return false;
    outstanding += 1;
    return true;
  }
</script>

{#if waking || spawning}
  <!-- Each state below is the column in that state, so each carries the
       column's own rules: without them the copy is the one thing on the page
       drawn at no padding and no gutter, while the list beside it is not. -->
  <div class="conv">
    <!-- A seat that is coming up, either side of the roster noticing: a
         spawn the core is running, or - for a LEAD the roster does not name
         yet - the one this page dispatched on open. Both draw the waking
         line, so the wake reads as one state rather than two. A worker's
         seat the roster does not name has no spawn coming toward it - only
         its lead can start it - and its line claims nothing. -->
    {#if spawning || slot.label === 'lead'}
      <div class="hold"><span class="shimmer">Waking up agent...</span></div>
    {:else}
      <div class="hold off">
        not running
        <span class="sub">{reason ?? 'this seat has no session behind it'}</span>
      </div>
    {/if}
  </div>
{:else if held.refused !== null && held.turns.length === 0}
  <!-- A refusal with nothing drawn under it is the column in that state. With
       turns held it is a line above them instead, because the refusal belongs
       to the ask rather than to the conversation - replacing the list would
       take the reader's own history away and unmount the thing whose scroll
       asks again. -->
  <div class="conv">
    <p class="hold off">This forge would not answer for this conversation: {held.refused}</p>
  </div>
{:else if !held.loaded}
  <div class="conv">
    <!-- Connected, and the first page has not come back. It is its own state:
         the empty copy here would say the seat has no history when the truth
         is that nothing has answered yet. -->
    <p class="hold">Reading the conversation...</p>
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
       them outside the thing that scrolls. -->
  <VList
    class="conv"
    data={held.turns}
    getKey={(turn: HeldTurn) => turn.key}
    itemProps={({ item }: { item: HeldTurn }) =>
      item.key === arriving ? { class: 'arriving' } : undefined}
    {shift}
    onscroll={scrolled}
    {@attach scrollViewport}
    bind:this={list}
  >
    {#snippet children(turn: HeldTurn)}
      <div
        class="turn"
        style:min-height={turn.held ? `${rowSizes.sizeOf(turn.key) ?? ROW_ESTIMATE}px` : undefined}
        {@attach (node: HTMLElement) => measureTurn(node, turn.key)}
      >
        <Turn
          {turn}
          {slot}
          {history}
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
        anchor = null;
        land();
      }}
    >
      <Icon name="down" />
    </button>
  {/if}
{/if}
<!-- The waiting prompts and the strip stand in EVERY conversation state,
     not only under the list: a seat coming up, refused, not yet read or
     empty holds the same tree, tasks and watchers as one mid-turn, and the
     rows are the whole way into them. Last in the column, so the strip
     keeps its place right above the box. -->
{#if queue !== undefined}{@render queue()}{/if}
<!-- Outside the list rather than in it, which is what makes the row pinned:
     the turns scroll under it, and the answer to whether the turn is still
     being written stops depending on where the reader is looking. It is a
     sibling of the scroller rather than a row of the grid, so the composer
     and the dock - both drawn under this column - never have to know it. -->
<Pinned info={pinned?.info ?? null} {connection} {slot} />

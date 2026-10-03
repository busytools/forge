<script lang="ts">
  import { untrack } from 'svelte';
  import { SvelteMap } from 'svelte/reactivity';
  import { VList, type VListHandle } from 'virtua/svelte';

  import Icon from '../components/Icon.svelte';
  import { subjectKey } from '../protocol';
  import { scrollAsk } from '../session/scroll-ask';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import Compacting from './Compacting.svelte';
  import { latestCompaction } from './compaction-jump';
  import {
    Chat,
    NOTHING,
    beingWritten,
    type Conversation,
    type Turn as HeldTurn,
  } from './conversation';
  import Echo from './Echo.svelte';
  import { echoes, ownWords } from './echoes.svelte';
  import Pinned from './Pinned.svelte';
  import Turn from './Turn.svelte';
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
    reason = null,
    compacting = false,
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The roster holds no session for this seat, which is its own state. */
    waking?: boolean;
    /** Why, when it does. */
    reason?: string | null;
    /** A compaction in flight, which the newest turn draws a line for. */
    compacting?: boolean;
  } = $props();

  /** How near the top the reader has to be before the turns above are asked for. */
  const REACH = 400;

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
  /** The newest turn's key: the row a compaction in flight belongs under. */
  const newest = $derived(newestTurn?.key ?? null);

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
    // The newest turn's own state, which is what says whether this send starts
    // a turn: one retried into a turn already running is not taken by it, so
    // the mark holds until the words themselves arrive.
    echoes.post(seat, held.words, newestTurn?.running === true);
    try {
      // Fire-and-forget like the composer's own send: the outcome rides the
      // subscription rather than a reply, so there is nothing to await.
      void connection.dispatch({ prompt: { key: slot, text: held.words, attachments: [] } });
    } catch {
      // A closed socket throws rather than answering, so the row says why
      // rather than the words going with a command that never left.
      echoes.refuse(seat, 'the socket is closed');
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
   * effect below depend on the entry the subscription writes on every frame -
   * which is a column that re-keys itself, and a `placed` that resets, under
   * each one.
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
  /** The content height at the last scroll event, which tells a reader moving from a layout moving. */
  let shaped = 0;
  /** The tick a settling compensation waits on, held so a later one can replace it. */
  let timer: ReturnType<typeof setTimeout> | null = null;

  const shift = $derived(outstanding > 0 || settling);

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
  function scrollViewport(node: HTMLElement): () => void {
    viewport = node;
    const content = node.firstElementChild;
    const watcher = new ResizeObserver(() => {
      if (holdsEverything()) working?.following(true);
      if (held.following) land();
    });
    watcher.observe(node);
    if (content !== null) watcher.observe(content);
    return () => {
      watcher.disconnect();
      viewport = null;
    };
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
      // Written to the seat's own entry rather than straight to `held`: a
      // conversation kept for a seat the reader has left must not draw.
      const unsubscribe = chat.value.subscribe((value) => {
        kept.set(which, value);
        if (untrack(() => seat) === which) held = value;
      });
      const stop = chat.start();
      entry = {
        connection: open,
        chat,
        stop: () => {
          unsubscribe();
          stop();
        },
      };
      live.set(which, entry);
    }
    working = entry.chat;
    // The placement belonged to the conversation that is going.
    placed = null;
    // The seat coming on screen is put there from what was kept, not from a
    // read, which is the whole point of holding it.
    held = untrack(() => kept.get(which)) ?? NOTHING;
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

  // The timer goes with the column, which is the one thing the effect above
  // cannot do for itself.
  $effect(() => () => {
    if (timer !== null) clearTimeout(timer);
  });

  /**
   * What the follow watches: the newest row, and the line that grows it.
   *
   * **The compaction line is part of the last row and arrives as a PROP**, not
   * as a frame, so a flip alone grows that row by its height with no scroll
   * behind it - and nothing else re-runs the follow until some later frame
   * happens to land, which on a session with no hooks is never. The line then
   * draws with its baseline below the fold for the whole compaction. Keyed
   * here so the follow re-sticks when the line appears.
   */
  const follows = $derived(held.turns.length === 0 ? null : `${held.turns.length}:${compacting}`);

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
    const units = fold(turn.messages, slot, beingWritten(turn));
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
  function land(): void {
    if (viewport === null) return;
    viewport.scrollTop = viewport.scrollHeight;
    // Where the column last left the reader, which is the foot the browser
    // clamped the pin to. A scroll event at this offset is the pin's own echo.
    placed = viewport.scrollTop;
  }

  /** The ask already answered, so a repeat of the same token is not acted on twice. */
  let answeredAsk: number | null = null;
  /** An ask still being worked: its row is not loaded yet, and pages are being pulled. */
  let asking = false;

  /**
   * The header's ask: reveal the latest compaction.
   *
   * Through the list's handle rather than the element, because the row may
   * not be drawn - and the scroll it performs fires the same scroll event a
   * reader's own wheel does, so the follow turns off exactly the way it does
   * when anyone scrolls away from the foot. Nothing else has to remember it.
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
    // **And once more after this frame's layout.** The foot a pin asks for is
    // the one that is true at the moment it asks, and a row's own content -
    // code, a disclosure opening, a table - is laid out after this column's
    // effects have run. On a seat whose history is already written that is the
    // whole of the difference between opening at the newest turn and opening
    // most of a screen above it.
    const settled = requestAnimationFrame(land);
    return () => cancelAnimationFrame(settled);
  });

  /** Where the reader is, and whether they have reached the top. */
  function scrolled(offset: number): void {
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
    if (!shrank) {
      if (atFoot()) working?.following(true);
      // `placed` is where the last pin left the reader; before any pin has
      // run it is unknown, and a reader above the foot is above it whatever
      // that number is - so the comparison falls back to any upward move
      // rather than never disarming, which left the way-back hidden on a
      // column that had not pinned yet (Ved, 2026-10-03).
      else if (offset < (placed ?? Infinity)) working?.following(false);
    }
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

{#if waking}
  <!-- Each state below is the column in that state, so each carries the
       column's own rules: without them the copy is the one thing on the page
       drawn at no padding and no gutter, while the list beside it is not. -->
  <div class="conv">
    <!-- A seat with no session behind it. It claims nothing about a spawn:
         this page cannot start one, and a line saying one is coming would be
         a promise no code keeps. -->
    <div class="hold off">
      not running
      <span class="sub">{reason ?? 'this seat has no session behind it'}</span>
    </div>
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
    {#if compacting}
      <Compacting />
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
    {#if compacting}
      <Compacting />
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
    {shift}
    onscroll={scrolled}
    {@attach scrollViewport}
    bind:this={list}
  >
    {#snippet children(turn: HeldTurn)}
      <div class="turn">
        <Turn
          {turn}
          {slot}
          compacting={compacting && turn.key === newest}
          carried={turn.key === newest ? (pinned?.key ?? null) : null}
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
  <!-- Outside the list rather than in it, which is what makes the row pinned:
       the turns scroll under it, and the answer to whether the turn is still
       being written stops depending on where the reader is looking. It is a
       sibling of the scroller rather than a row of the grid, so the composer
       and the dock - both drawn under this column - never have to know it. -->
  <Pinned info={pinned?.info ?? null} />
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
        land();
      }}
    >
      <Icon name="down" />
    </button>
  {/if}
{/if}

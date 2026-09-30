<script lang="ts">
  import { VList, type VListHandle } from 'virtua/svelte';

  import { subjectKey } from '../protocol';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import Compacting from './Compacting.svelte';
  import { Chat, NOTHING, type Conversation, type Turn as HeldTurn } from './conversation';
  import Turn from './Turn.svelte';

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
    cwd,
    waking = false,
    reason = null,
    compacting = false,
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The session's working tree, which a call's target is named against. */
    cwd: string | null;
    /** The roster holds no session for this seat, which is its own state. */
    waking?: boolean;
    /** Why, when it does. */
    reason?: string | null;
    /** A compaction in flight, which the newest turn draws a line for. */
    compacting?: boolean;
  } = $props();

  /** How near the top the reader has to be before the turns above are asked for. */
  const REACH = 400;

  let held = $state<Conversation>(NOTHING);
  let list = $state<VListHandle | null>(null);
  let working: Chat | null = null;
  let first = true;
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
  /** The newest turn's key: the row a compaction in flight belongs under. */
  const newest = $derived(held.turns[held.turns.length - 1]?.key ?? null);
  let opened: { seat: string; connection: Connection; stop: () => void } | null = null;
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
  /** The tick a settling compensation waits on, held so a later one can replace it. */
  let timer: ReturnType<typeof setTimeout> | null = null;

  const shift = $derived(outstanding > 0 || settling);

  $effect(() => {
    const which = seat;
    const open = connection;
    if (opened !== null && opened.seat === which && opened.connection === open) return;
    opened?.stop();
    const chat = new Chat(open, slot);
    working = chat;
    const unsubscribe = chat.value.subscribe((value) => {
      held = value;
    });
    const stop = chat.start();
    opened = {
      seat: which,
      connection: open,
      stop: () => {
        unsubscribe();
        stop();
        working = null;
      },
    };
  });

  // The column's own teardown, which the effect above cannot do: it stops a
  // conversation only to put the next one in its place.
  $effect(() => () => {
    opened?.stop();
    opened = null;
  });

  // The first page is drawn at the end rather than the start. It runs once per
  // conversation: after that the reader's own scroll position is the truth,
  // and a second jump would take them away from where they had scrolled to.
  $effect(() => {
    if (!first || !held.loaded || held.turns.length === 0) return;
    first = false;
    list?.scrollToIndex(held.turns.length - 1, { align: 'end' });
  });

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

  // A reader at the end FOLLOWS the newest turn: that is what the end of a
  // conversation means, and a page that grew without the view moving would
  // lose the very thing it was opened on. A reader anywhere else is left
  // exactly where they are.
  $effect(() => {
    if (follows === null || !held.loaded || !held.atEnd) return;
    list?.scrollToIndex(held.turns.length - 1, { align: 'end' });
  });

  /** Where the reader is, and whether they have reached the top. */
  function scrolled(offset: number): void {
    const total = list?.getScrollSize() ?? 0;
    const viewport = list?.getViewportSize() ?? 0;
    working?.position(offset + viewport >= total - 8);
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
  function loadOlder(): void {
    if (working?.older() !== true) return;
    outstanding += 1;
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
{:else if held.refused !== null}
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
    <div class="hold">
      Nothing said yet
      <span class="sub">this seat has no history: what is said here starts it</span>
    </div>
    {#if compacting}
      <Compacting />
    {/if}
  </div>
{:else}
  <!-- The list draws its own scroll viewport, so the sheet's `.conv` rules go
       on that element rather than a wrapper around it: they are the column's
       padding, its scrollbar gutter and its scrollbar, and a wrapper would put
       them outside the thing that scrolls. -->
  <VList
    bind:this={list}
    class="conv"
    data={held.turns}
    getKey={(turn: HeldTurn) => turn.key}
    {shift}
    onscroll={scrolled}
  >
    {#snippet children(turn: HeldTurn)}
      <div class="turn">
        <Turn {turn} {cwd} compacting={compacting && turn.key === newest} />
      </div>
    {/snippet}
  </VList>
{/if}

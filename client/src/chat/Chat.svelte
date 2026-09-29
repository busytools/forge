<script lang="ts">
  import { VList, type VListHandle } from 'virtua/svelte';

  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
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
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The session's working tree, which a call's target is named against. */
    cwd: string | null;
    /** The roster holds no session for this seat, which is its own state. */
    waking?: boolean;
    /** Why, when it does. */
    reason?: string | null;
  } = $props();

  /** How near the top the reader has to be before the turns above are asked for. */
  const REACH = 400;

  let held = $state<Conversation>(NOTHING);
  let list = $state<VListHandle | null>(null);
  let working: Chat | null = null;
  let first = true;
  /**
   * Whether the list should hold its scroll position from the end.
   *
   * **Only while older turns are arriving**, which is the case the
   * compensation is written for. `virtua` moves the offset by the height of
   * whatever was added, and it does that for ANY addition: with it on, a turn
   * arriving below a reader who has scrolled up moves them by that row's
   * height, which is the one thing this page must not do. So it goes on for
   * the prepend and comes off again once the list has taken it.
   */
  let shift = $state(false);

  $effect(() => {
    const chat = new Chat(connection, slot);
    working = chat;
    const unsubscribe = chat.value.subscribe((value) => {
      held = value;
    });
    const stop = chat.start();
    return () => {
      unsubscribe();
      stop();
      working = null;
    };
  });

  // The first page is drawn at the end rather than the start. It runs once per
  // conversation: after that the reader's own scroll position is the truth,
  // and a second jump would take them away from where they had scrolled to.
  $effect(() => {
    if (!first || !held.loaded || held.turns.length === 0) return;
    first = false;
    list?.scrollToIndex(held.turns.length - 1, { align: 'end' });
  });

  // A prepend turns the compensation on, and the tick after it lands turns it
  // off again: the list has taken the rows by then, and leaving it on would
  // make the next appended turn move the reader.
  $effect(() => {
    if (held.prepends === 0) return;
    shift = true;
    const timer = setTimeout(() => {
      shift = false;
    }, 0);
    return () => clearTimeout(timer);
  });

  // A reader at the end FOLLOWS the newest turn: that is what the end of a
  // conversation means, and a page that grew without the view moving would
  // lose the very thing it was opened on. A reader anywhere else is left
  // exactly where they are.
  $effect(() => {
    if (!held.loaded || !held.atEnd || held.turns.length === 0) return;
    list?.scrollToIndex(held.turns.length - 1, { align: 'end' });
  });

  /**
   * Where the reader is, and whether they have reached the top.
   *
   * `shift` on the list is what keeps their place when older turns are
   * prepended: without it the list measures from the start, so every prepend
   * pushes what they are reading down by the height of what arrived above it.
   */
  function scrolled(offset: number): void {
    const total = list?.getScrollSize() ?? 0;
    const viewport = list?.getViewportSize() ?? 0;
    working?.position(offset + viewport >= total - 8);
    if (offset < REACH) working?.older();
  }
</script>

{#if waking}
  <!-- A seat with no session behind it. It claims nothing about a spawn: this
       page cannot start one, and a line saying one is coming would be a
       promise no code keeps. -->
  <div class="hold off">
    not running
    <span class="sub">{reason ?? 'this seat has no session behind it'}</span>
  </div>
{:else if held.refused !== null}
  <p class="hold off">This forge would not answer for this conversation: {held.refused}</p>
{:else if !held.loaded}
  <!-- Connected, and the first page has not come back. It is its own state:
       the empty copy here would say the seat has no history when the truth is
       that nothing has answered yet. -->
  <p class="hold">Reading the conversation...</p>
{:else if held.turns.length === 0}
  <div class="hold">
    Nothing said yet
    <span class="sub">this seat has no history: what is said here starts it</span>
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
    {#snippet children(turn)}
      <div class="turn">
        <Turn {turn} {cwd} />
      </div>
    {/snippet}
  </VList>
{/if}

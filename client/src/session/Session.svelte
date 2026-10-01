<script lang="ts">
  import type { Snippet } from 'svelte';

  import type { Connection } from '../socket';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import AccountChip from './AccountChip.svelte';
  import Inspector from './Inspector.svelte';
  import Rail from './Rail.svelte';
  import { watchSession, type SessionRead } from './live';
  import {
    accountChip,
    headerFacts,
    seatState,
    type ComposerProps,
    type ConversationProps,
  } from './view';
  import type { SessionRecord } from './wire';

  /**
   * The session page: three columns over one sheet, and the box beneath them.
   *
   * **The page owns the subscriptions and hands the other two columns their
   * data.** The conversation and the composer are their own tasks' trees, so
   * they arrive as snippets rather than as imports: a page that imported them
   * would not build until both had landed, and this branch has to stand on its
   * own.
   */
  let {
    slot,
    connection,
    wire,
    conversation = null,
    composer = null,
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The home's snapshot, which the rail, the header and four sections read. */
    wire: HomeWire;
    /** The conversation column. Absent, the seat's own not-running state draws. */
    conversation?: Snippet<[ConversationProps]> | null;
    /** The box under it, which replaces itself while the seat cannot take keys. */
    composer?: Snippet<[ComposerProps]> | null;
  } = $props();

  // Raw: a read answers with a whole new record, so nothing here is mutated in
  // place, and `$state` would re-proxy the tree it is handed on every frame.
  let read = $state.raw<SessionRead>({ wire: null, refused: null });
  $effect(() => {
    // Read here rather than through `$derived`, so a seat change re-subscribes
    // and the store the page left is let go with the last subscriber.
    const open = connection;
    const seat = slot;
    // The page answers for a seat only when it can: the dock that answers a
    // prompt lives in the composer, and declaring the role without one parks
    // every prompt on a reply nothing sends.
    return watchSession(open, seat, composer !== null).subscribe(($next) => {
      read = $next;
    });
  });

  const record: SessionRecord | null = $derived(read.wire);
  const seat = $derived(seatState(wire, slot));
  const facts = $derived(record === null ? null : headerFacts(record.header));
  const account = $derived(accountChip(wire, slot));

  /**
   * Whether each rail is shown, and the two widths a rail stops being a
   * column at.
   *
   * The server held this in hidden checkboxes, because a page with no script
   * had nowhere else to put it - and the same box meant `hidden` above the
   * breakpoint and `shown` below it, which is a control that cannot be
   * labelled honestly. Here the state is what it says, the default is each
   * band's own, and the sheet turns a shown rail into a sheet below the step
   * rather than inverting the word.
   */
  const BELOW_980 = '(max-width: 980px)';
  const BELOW_1280 = '(max-width: 1280px)';
  const hasDom = typeof window !== 'undefined' && typeof window.matchMedia === 'function';
  const matches = (query: string) => hasDom && window.matchMedia(query).matches;

  let narrow = $state(matches(BELOW_980));
  let inspectorNarrow = $state(matches(BELOW_1280));

  $effect(() => {
    if (!hasDom) return;
    const rails = window.matchMedia(BELOW_980);
    const inspector = window.matchMedia(BELOW_1280);
    const sync = () => {
      narrow = rails.matches;
      inspectorNarrow = inspector.matches;
    };
    rails.addEventListener('change', sync);
    inspector.addEventListener('change', sync);
    return () => {
      rails.removeEventListener('change', sync);
      inspector.removeEventListener('change', sync);
    };
  });

  // `null` is "whatever this band does by default", which is the state a
  // reader who has never touched the handles is in.
  let leftChosen = $state<boolean | null>(null);
  let rightChosen = $state<boolean | null>(null);
  const leftShown = $derived(leftChosen ?? !narrow);
  const rightShown = $derived(rightChosen ?? !inspectorNarrow);

  // One clock for the page: every age and every countdown reads against the
  // same now, so two rows a second apart cannot draw the same age differently.
  // Age is a function of time rather than of data, so a re-read needs an
  // update the fleet may never send.
  let now = $state(Date.now());
  $effect(() => {
    now = Date.now();
    const tick = setInterval(() => {
      now = Date.now();
    }, 30_000);
    return () => clearInterval(tick);
  });

  const conversationProps = $derived<ConversationProps>({
    cwd: record?.state.scan_cwd ?? '',
    waking: seat.waking,
    reason: seat.reason,
    compacting: record?.composer.compacting ?? false,
    slot,
    connection,
  });
</script>

<div
  class="app"
  class:left-shown={leftShown}
  class:left-hidden={!leftShown}
  class:right-shown={rightShown}
  class:right-hidden={!rightShown}
>
  <Rail home={wire} current={slot} {now} onclose={() => (leftChosen = false)} />

  <main class="chat">
    <div class="sess">
      <!-- The handle to each column lives with the title, so a folded column
           leaves no edge behind and its control stays reachable. -->
      <button
        class="rail-tog tog-l"
        type="button"
        title="projects"
        aria-label="projects"
        aria-expanded={leftShown}
        onclick={() => (leftChosen = !leftShown)}
      >
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"><path d="M3 6h18M3 12h18M3 18h18" /></svg
        >
      </button>
      <button
        class="rail-tog tog-r"
        type="button"
        title="inspector"
        aria-label="inspector"
        aria-expanded={rightShown}
        onclick={() => (rightChosen = !rightShown)}
      >
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          ><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M15 4v16" /></svg
        >
      </button>
      <span class="dot {seat.mark}"></span>
      <span class="nm">{seat.name}</span>
      <span class="mono dim">{slot.org}</span>
      <span class="facts">
        {#if facts !== null}
          <span><span class="fk">model</span> <span class="v">{facts.model}</span></span>
          <span class="sep">{'\u{b7}'}</span>
          <span><span class="fk">effort</span> <span class="v">{facts.effort}</span></span>
          <span class="sep">{'\u{b7}'}</span>
          <span>
            <span class="fk">mode</span>
            {#if facts.mode !== null}<span class="perm {facts.mode.klass}">{facts.mode.wire}</span
              >{:else}<span class="perm">{'\u{2014}'}</span>{/if}
          </span>
          <span class="sep">{'\u{b7}'}</span>
          <span class="cm">
            <span class="fk">ctx</span>
            <!-- The track is drawn only for a usage that was reported. An empty
                 track stands for an unknown value as readily as for a real zero,
                 and nothing in the record says which of the two this is. -->
            {#if facts.percent !== null}
              <span class="tk"><span class="fl" style={`width:${facts.percent}%`}></span></span>
            {/if}
            <span class="v">{facts.percent === null ? '\u{2014}' : `${facts.percent}%`}</span>
          </span>
        {/if}
        {#if account !== null}<AccountChip view={account} />{/if}
      </span>
    </div>

    <!-- The column's element is the chat's own where a chat is mounted: it
         draws its own scroll viewport, and the sheet's `.conv` rules are that
         viewport's padding and scrollbar, so a wrapper carrying them as well
         would put a second scroller outside the one that scrolls. -->
    {#if conversation !== null}
      {@render conversation(conversationProps)}
    {:else if seat.waking}
      <!-- The seat's own state, which this column owns: a seat nothing is
           running behind says so rather than drawing an empty page. -->
      <div class="conv">
        <div class="hold off">
          not running
          <span class="sub">{seat.reason ?? 'this seat has no session behind it'}</span>
        </div>
      </div>
    {/if}
  </main>

  <Inspector {wire} {record} {slot} {now} onclose={() => (rightChosen = false)} />

  {#if composer !== null && record !== null}
    <div class="composer">{@render composer({ record, slot, seat, connection })}</div>
  {/if}
</div>

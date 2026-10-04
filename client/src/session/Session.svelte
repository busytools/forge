<script lang="ts">
  import type { Snippet } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';

  import Brand from '../components/Brand.svelte';
  import Icon from '../components/Icon.svelte';
  import { subjectKey } from '../protocol';
  import { report, type Connection } from '../socket';
  import type { HomeWire } from '../wire/home';
  import type { SessionSlot } from '../wire/types';
  import CopyButton from './CopyButton.svelte';
  import Inspector from './Inspector.svelte';
  import Rail from './Rail.svelte';
  import { chosenAfterPop, railEntry, railOnTop, type RailSide } from './rail-history';
  import SessionId from './SessionId.svelte';
  import Queue from '../chat/Queue.svelte';
  import { watchSession, type SessionRead } from './live';
  import { askCompaction } from './scroll-ask';
  import {
    compactionFigure,
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
    mark = null,
    conversation = null,
    composer = null,
  }: {
    slot: SessionSlot;
    connection: Connection;
    /** The home's snapshot, which the rail, the header and four sections read. */
    wire: HomeWire;
    /** The brand the header's wordmark draws, as the home's own brand does. */
    mark?: string | null;
    /** The conversation column. Absent, the seat's own not-running state draws. */
    conversation?: Snippet<[ConversationProps]> | null;
    /** The box under it, which replaces itself while the seat cannot take keys. */
    composer?: Snippet<[ComposerProps]> | null;
  } = $props();

  // Raw: a read answers with a whole new record, so nothing here is mutated in
  // place, and `$state` would re-proxy the tree it is handed on every frame.
  let read = $state.raw<SessionRead>({ wire: null, refused: null });
  $effect(() => {
    // Read here rather than through `$derived`, so a seat change subscribes the
    // seat it moved to - the seat it left keeps its record and its subscription.
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
  /** The count the folded panel names, where the row's unit draws the figure. */
  const compactionCount = $derived(record?.conversation.compaction_count ?? 0);

  /**
   * The seams this page has already asked the core to start.
   *
   * The terminal refuses a second click on a seat that is mid-spawn for the
   * same reason: a second ask races the first, and the seat scrambles. Here
   * the ask is an effect rather than a click handler, so it re-runs on every
   * frame that touches the roster - and the roster names a seat only once
   * `Spawning` has landed, so without this the ask would repeat until then.
   *
   * Nothing draws from it, so an ordinary `Set` would do - the reactive one
   * is what the sheet's lint takes for a mutable `Set`, and it costs nothing
   * here.
   */
  const asked = new SvelteSet<string>();

  /**
   * Start a lead nothing is running behind, which is the terminal's own rail
   * click (`switch_to_project_lead`). A seat the roster already names is up or
   * on its way, and switching to it is what the route this page is on already
   * did.
   *
   * A project's own lead is the one seat the core can start by name: a worker
   * is spawned by the lead that owns it, so a worker seat with nothing behind
   * it stays as it is rather than asking for a spawn the core cannot place.
   *
   * The only refusal this can meet is the socket's own - the ask names a
   * project the roster carries, so the core has one to start, and its refusal
   * for a project it does not know cannot be reached from here.
   */
  $effect(() => {
    const open = connection;
    const seatSlot = slot;
    if (seatSlot.label !== 'lead' || !seat.waking) return;
    const key = subjectKey({ session: seatSlot });
    if (asked.has(key)) return;
    asked.add(key);
    try {
      void open.dispatch({
        spawn_project: { project_name: seatSlot.project, launch_settings: {} },
      });
    } catch (error) {
      // A closed socket has nothing to start: the seat keeps drawing its
      // not-running state, which is the truth about it - and the click did
      // nothing, so it is reported rather than swallowed.
      report('the spawn was not sent', error);
      asked.delete(key);
    }
  });
  // Beside the context figure, which is the row the terminal draws it on: the
  // count belongs to the conversation and not to the header, and the row is
  // where a reader looks for it.
  const compactions = $derived(
    record === null ? null : compactionFigure(record.conversation.compaction_count),
  );

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

  /**
   * A covering rail opens onto history, so Back closes what it opened; a rail
   * shown as a column takes no entry. The push and the pop's meaning are
   * `rail-history.ts`'s, so a click is not what they rest on.
   */
  let lastRail: RailSide | null = hasDom ? railOnTop(history.state) : null;
  let closedUnder: RailSide | null = null;

  function openRail(side: RailSide) {
    if (side === 'left' ? narrow : inspectorNarrow) {
      // One entry covers the open state: a second rail opened over the first
      // joins it rather than stacking a step of its own.
      if (lastRail === null) {
        history.pushState(railEntry(history.state, side), '', location.href);
        lastRail = side;
      }
    }
    closedUnder = null;
    if (side === 'left') leftChosen = true;
    else rightChosen = true;
  }

  function closeRail(side: RailSide) {
    if (side === 'left') leftChosen = false;
    else rightChosen = false;
    // A covering rail still open keeps the entry earned: the press closes the
    // side it names, and the step stays for the Back that closes the last one.
    const stillCovering = side === 'left' ? inspectorNarrow && rightShown : narrow && leftShown;
    if (stillCovering) return;
    if (railOnTop(history.state) === side) {
      history.back();
      return;
    }
    // The entry sits beneath another navigation's, where no pop can reach it
    // without leaving that page: it is stepped down instead, and the Back
    // that walks past it drops it without re-opening anything.
    closedUnder = side;
  }

  $effect(() => {
    const restore = () => {
      const landed = railOnTop(history.state);
      if (landed !== null && landed === closedUnder) {
        // The inert entry a close stepped down: dropped here, so it is
        // consumed rather than left as a step that shows nothing.
        closedUnder = null;
        lastRail = null;
        history.back();
        return;
      }
      const chosen = chosenAfterPop(history.state, narrow, lastRail, closedUnder);
      lastRail = landed;
      closedUnder = null;
      if (chosen === null) return;
      leftChosen = chosen.left;
      rightChosen = chosen.right;
    };
    addEventListener('popstate', restore);
    return () => removeEventListener('popstate', restore);
  });

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
  <Rail home={wire} current={slot} {now} {connection} onclose={() => closeRail('left')} />

  <main class="chat">
    <div class="sess">
      <!-- The wordmark, the way home: the home's own brand a size down so the
           session's name still leads, in a real anchor so the keyboard reaches
           it, the URL stays real and the router follows it in place. The
           hairline is what separates the app's brand from the seat's own. -->
      <a class="brand" href="/" title="home">
        <Brand name={mark} />
        <span class="word">forge</span>
      </a>
      <span class="mastsep" aria-hidden="true"></span>
      <!-- The handle to each column lives with the title, so a folded column
           leaves no edge behind and its control stays reachable. -->
      <button
        class="rail-tog tog-l"
        type="button"
        title="projects"
        aria-label="projects"
        aria-expanded={leftShown}
        onclick={() => (leftShown ? closeRail('left') : openRail('left'))}
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
        onclick={() => (rightShown ? closeRail('right') : openRail('right'))}
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
      <span class="mono dim f-org">{slot.org}</span>
      <span class="facts">
        {#if facts !== null}
          {#if facts.sessionId !== null}
            <span class="fact f-session"><SessionId id={facts.sessionId} /></span>
          {/if}
          <!-- Effort rides the model it belongs to: a property of that choice,
               so it reads as the choice's suffix rather than a fact of its own,
               with the whole reading on the control's title. -->
          <span
            class="fact f-model"
            title={`model ${facts.model} ${'\u{b7}'} effort ${facts.effort}`}
            ><span class="fk">model</span> <span class="v">{facts.model}</span>
            <span class="eff">{facts.effort}</span></span
          >
          <span class="fact f-mode">
            <span class="fk">mode</span>
            {#if facts.mode !== null}<span class="perm {facts.mode.klass}">{facts.mode.wire}</span
              >{:else}<span class="perm">{'\u{2014}'}</span>{/if}
          </span>
          <!-- The conversation's context is one unit: how full it is, and how
               many times it has been cut. The track is drawn only for a usage
               that was reported - an empty track stands for an unknown value as
               readily as for a real zero, and nothing in the record says which
               of the two this is. -->
          <span class="cm fact f-ctx">
            <span class="fk">ctx</span>
            {#if facts.percent !== null}
              <span class="tk"><span class="fl" style={`width:${facts.percent}%`}></span></span>
            {/if}
            <span class="v">{facts.percent === null ? '\u{2014}' : `${facts.percent}%`}</span>
            {#if compactions !== null}
              <!-- Tappable: the count is a fact about the conversation, and
                   the one thing a reader wants from it is to see the latest
                   cut - so the click takes them there. -->
              {'\u{b7}'}
              <button
                class="f-comp"
                type="button"
                title="go to the latest compaction"
                onclick={askCompaction}>{compactions}</button
              >
            {/if}
          </span>
        {/if}
      </span>
      {#if facts !== null}
        <!-- Everything the row folds, one tap away: the facts stay reachable at
             every width, which is what keeps the collapse honest. -->
        <details class="more">
          <summary title="every fact"><Icon name="dots" /></summary>
          <div class="mfacts">
            {#if facts.sessionId !== null}
              <div class="kv">
                <span class="k">session</span>
                <span class="v">{facts.sessionId}<CopyButton id={facts.sessionId} /></span>
              </div>
            {/if}
            <div class="kv"><span class="k">model</span><span class="v">{facts.model}</span></div>
            <div class="kv"><span class="k">effort</span><span class="v">{facts.effort}</span></div>
            <div class="kv">
              <span class="k">mode</span>
              <span class="v"
                >{#if facts.mode !== null}<span class="perm {facts.mode.klass}"
                    >{facts.mode.wire}</span
                  >{:else}<span class="perm">{'\u{2014}'}</span>{/if}</span
              >
            </div>
            <div class="kv">
              <span class="k">ctx</span>
              <span class="v">{facts.percent === null ? '\u{2014}' : `${facts.percent}%`}</span>
            </div>
            <div class="kv">
              <span class="k">compactions</span><span class="v">{compactionCount}</span>
            </div>
          </div>
        </details>
      {/if}
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

  <Inspector {wire} {record} {slot} {now} onclose={() => closeRail('right')} />

  {#if composer !== null && record !== null}
    <div class="composer">
      <!-- The queue sits between the pinned turn row (drawn by the chat
           column above) and the box: what is waiting, then what you type. -->
      <Queue rows={record.queue} ended={record.queue_ended} {slot} {connection} />
      {@render composer({ record, slot, seat, connection })}
    </div>
  {/if}
</div>

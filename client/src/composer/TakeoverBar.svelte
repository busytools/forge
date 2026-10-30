<script lang="ts">
  /**
   * The browser takeover's bar: back to chat, the parked ask's words, Done,
   * Not now, and the feedback line for what did not work.
   *
   * **Drawn by the composer, outside its own container** (`.surf` is a
   * `container-type`, which makes it the containing block for fixed children),
   * so the bar sits at the window's top above the native page layer. **The ask
   * rides the bar** because its card is drawn in the chat behind the page: the
   * words are here whenever the answers are. The feedback line is the dock's
   * own words row - the same Field, TakeCard and mic - so dictation here is
   * the composer's, not a second implementation. **What the take is doing is
   * the bar's to say**: the composer's band and notice rows are behind the
   * picture while the takeover is up, so the open and a refusal draw above
   * this row.
   */
  import Icon from '../components/Icon.svelte';
  import Field from './Field.svelte';
  import TakeCard from './TakeCard.svelte';
  import { tabRows } from '../browser/host';
  import { holdsControl, selectTab, surface, takeControl } from '../browser/surface.svelte';
  import type { NavState } from '../browser/surface.svelte';
  import type { RowPhase } from './dictate.svelte';
  import type { Take } from './wire';

  let {
    title,
    reason = null,
    notes = $bindable(''),
    take = null,
    phase = null,
    wire = null,
    silent = null,
    nav = { can_back: false, can_forward: false },
    onabandon = () => {},
    onmic = () => {},
    dictation = false,
    takeline = null,
    opening = false,
    ondismiss = () => {},
    ondone = null,
    onnotnow = null,
    onback,
    onnav = null,
    onfeedback = null,
    cansend = true,
    onretry = null,
  }: {
    title: string;
    /** The parked ask's own words; null when no ask rides this seat's page. */
    reason?: string | null;
    notes?: string;
    take?: Take | null;
    /**
     * What the take row is doing, from `takePhase`: **the page's own take
     * counts, not only the record's**. The record's take is the socket's echo,
     * so a take whose
     * stream has not gone out yet - a socket still opening, or the round trip
     * of every press - has no record and would draw an empty field here while
     * the composer's own row already draws its card.
     */
    phase?: RowPhase | null;
    wire?: {
      frames: number;
      bytes: number;
      rate: number | null;
      dbfs: number[];
      elapsedMs: number;
    } | null;
    silent?: string | null;
    /** What the page's own history allows; the shell pushes it as it moves. */
    nav?: NavState;
    onabandon?: () => void;
    onmic?: () => void;
    dictation?: boolean;
    /**
     * What became of a take this page started, when the line is the bar's to
     * say: the feedback row is this surface's words row, and the composer's
     * own notice row is behind the picture while the takeover is up, so a
     * refusal routed there would be invisible exactly when the mic was pressed.
     *
     * **The line is the seat's, not the row's**, and it draws here whichever
     * row's press left it: the takeover covers the row that would have shown
     * it, and a seat's last take did or did not open regardless of which of
     * its mics was pressed.
     */
    takeline?: { tone: string; text: string } | null;
    /**
     * Whether the open has outlasted `OPEN_DRAWN_MS` - the composer's own
     * `openingShown`: false for the first 180ms and never true for an open
     * that lands inside it, so a quick press flashes nothing.
     *
     * **Nothing else on this surface says the open is running.** The card
     * draws from this page's own take, which exists only once the capture has
     * opened; until then the row keeps its field, mic and send and changes in
     * no way, so the press reads as dead.
     */
    opening?: boolean;
    /** The reader typed in the row, which dismisses the line above it the way
     *  the same typing dismisses the composer's own notice. */
    ondismiss?: () => void;
    /** The hand-off's answers; null once no ask is parked, leaving back-to-chat. */
    ondone?: (() => void) | null;
    onnotnow?: (() => void) | null;
    onback: () => void;
    /** The history arrows' click; their keys are the surface's own. */
    onnav?: ((direction: 'back' | 'forward') => void) | null;
    onfeedback?: ((text: string) => void) | null;
    /** Whether those words can be sent: only a page the ask names answers. */
    cansend?: boolean;
    /** Present while the surface failed: the door back to a live page. */
    onretry?: (() => void) | null;
  } = $props();

  let feedbackOpen = $state(false);
  /** The ask's words, or `null` when there are none: an empty reason draws no row. */
  const askWords = $derived(reason === null || reason.trim() === '' ? null : reason);
  /** The tab picker's own select: **its value is set here rather than by the
   *  select's binding**, which does not take when the options arrive after
   *  the viewed tab does. */
  let picker = $state<HTMLSelectElement | null>(null);
  $effect(() => {
    if (picker !== null && surface.tab !== null) picker.value = String(surface.tab);
  });
  let barEl = $state<HTMLDivElement | null>(null);

  // The page area starts under the bar; a row the bar grows by (a take's card)
  // would otherwise be drawn underneath the native picture.
  $effect(() => {
    const bar = barEl;
    if (bar === null) return;
    const set = (): void => {
      surface.barHeight = bar.getBoundingClientRect().height || 38;
    };
    set();
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(set);
    observer?.observe(bar);
    return () => {
      observer?.disconnect();
      surface.barHeight = 38;
    };
  });

  function send(): void {
    // The click and the Enter agree: a line that cannot answer sends nothing.
    if (!cansend) return;
    const text = notes.trim();
    if (text === '' || onfeedback === null) return;
    onfeedback(text);
  }

  /**
   * Whether this surface's gestures reach the page. **Exactly one viewer
   * holds the role**, and the server names whose; a surface that does not
   * hold it draws the page and sends nothing until the person takes it.
   */
  const holding = $derived(holdsControl());

  /** The viewed profile's tabs, as the server last pushed them: the same
   *  rows the strip draws, so the owner is the session's name and not the
   *  wire object. */
  const tabs = $derived.by(() => {
    if (surface.tabs === null) return [];
    try {
      return tabRows(surface.tabs);
    } catch {
      // The strip reports the unreadable list; the picker has nothing to
      // offer from it.
      return [];
    }
  });

  /**
   * One tab picked from the bar: a re-attach on that tab, which replaces what
   * this connection was watching. **A tab is never created or closed here** -
   * it belongs to the calls that drive it.
   */
  function pick(event: Event): void {
    const chosen = Number((event.currentTarget as HTMLSelectElement).value);
    if (!Number.isInteger(chosen) || chosen < 1) return;
    void selectTab(chosen);
  }
</script>

<div class="takeover-bar" role="region" aria-label="the browser takeover" bind:this={barEl}>
  <div class="tb-row">
    <button class="tb-btn d" type="button" onclick={onback}>back to chat</button>
    <span class="tb-nav" role="group" aria-label="the page's history">
      <button
        class="tb-btn nav"
        type="button"
        aria-label="back"
        title="back (Cmd+[)"
        disabled={!holding || !nav.can_back}
        onclick={() => onnav?.('back')}
      >
        <Icon name="back" />
      </button>
      <button
        class="tb-btn nav"
        type="button"
        aria-label="forward"
        title="forward (Cmd+])"
        disabled={!holding || !nav.can_forward}
        onclick={() => onnav?.('forward')}
      >
        <Icon name="forward" />
      </button>
    </span>
    <span class="tb-title">{title}</span>
    {#if tabs.length > 0}
      <select
        class="tb-pick"
        aria-label="the tab to view"
        title="the profile's tabs"
        disabled={surface.phase !== 'live' && surface.phase !== 'failed'}
        bind:this={picker}
        value={surface.tab === null ? '' : String(surface.tab)}
        onchange={pick}
      >
        {#each tabs as row (row.number)}
          <option value={row.number} selected={surface.tab === row.number}>
            {row.number} · {row.title !== ''
              ? row.title
              : row.url !== ''
                ? row.url
                : `tab ${row.number}`} · {row.owner}
          </option>
        {/each}
      </select>
    {/if}
    <span class="tb-acts">
      {#if surface.phase === 'live'}
        <button
          class="tb-btn"
          type="button"
          aria-pressed={holding}
          onclick={() => void takeControl(!holding)}
        >
          {holding ? 'release control' : 'take control'}
        </button>
      {/if}
      {#if onretry !== null}
        <button class="tb-btn" type="button" onclick={onretry}>try again</button>
      {/if}
      {#if ondone !== null && onnotnow !== null}
        <button class="tb-btn" type="button" onclick={ondone}>Done</button>
        {#if onfeedback !== null}
          <button
            class="tb-btn d"
            type="button"
            aria-expanded={feedbackOpen}
            onclick={() => (feedbackOpen = !feedbackOpen)}
          >
            something's wrong…
          </button>
        {/if}
        <button class="tb-btn d" type="button" onclick={onnotnow}>Not now</button>
      {/if}
    </span>
  </div>
  {#if askWords !== null}
    <!-- **The ask, on the bar that answers it.** The card carrying these
         words is drawn in the chat behind the native page, so with the page up
         this is the only place the person can read what is asked. -->
    <div class="tb-ask">{askWords}</div>
  {/if}
  {#if feedbackOpen && onfeedback !== null}
    {#if opening}
      <!-- The open, in the bar and only past the wait the props doc names:
           until this page's own take exists there is nothing else on the
           surface that says the press landed. -->
      <div class="tb-line opening">
        <span class="ring" aria-hidden="true"></span>opening the microphone…
      </div>
    {:else if takeline !== null}
      <!-- The reason a take pressed here did not open, in the bar: the
           composer's own row is behind the picture, so a line drawn there is
           a press that looks dead. -->
      <div class="tb-line {takeline.tone}">{takeline.text}</div>
    {/if}
    <div class="tb-feedback">
      {#if phase !== null}
        <TakeCard {take} {phase} {wire} oncancel={onabandon} {silent} spread />
      {:else}
        <Field
          editor="bar"
          class="tb-field"
          bind:value={notes}
          rows={1}
          placeholder="what did not work? the agent reads this"
          oninput={() => ondismiss()}
          onkeydown={(event: KeyboardEvent) => {
            event.stopPropagation();
            if (event.key === 'Enter') send();
          }}
        />
      {/if}
      {#if dictation}
        <button
          class="micb"
          type="button"
          aria-label={phase === null ? 'dictate the feedback' : 'stop dictating'}
          onclick={onmic}
        >
          <Icon name="mic" />
        </button>
      {/if}
      {#if cansend && phase === null && notes.trim() !== ''}
        <button
          class="sendb"
          type="button"
          aria-label="send the feedback and end the hand-off"
          onclick={send}
        >
          <Icon name="send" />
        </button>
      {/if}
    </div>
  {/if}
</div>

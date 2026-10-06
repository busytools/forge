<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { askReveal } from '../session/scroll-ask';
  import { duration } from './numbers';
  import { subagents, transcribable } from './subagents.svelte';
  import { panelStyle } from './strip-panel';
  import { firstLine } from './text';

  /**
   * The agents row in the strip above the composer: how many instances the
   * seat has out and how many it has finished, and the way into each one.
   *
   * **It rides the strip, so its lifetime is the strip's** - a turn being
   * written, plus the beat after it settles. Whether the row should outlive
   * the turn (a backgrounded instance can) is an open design call, not
   * something this component decides.
   *
   * It reads the same card join every dispatch row reads, so the counts, the
   * list and the rows can never disagree.
   *
   * **Only instances a transcript can be opened for are listed.** The row a
   * dispatch drew is where its transcript lives - and an instance that ran
   * before a restart or resume has no frames on the page, so its row would
   * open onto a brief and nothing. Such instances stay out of the list rather
   * than leading somewhere empty.
   */
  const MAX_ACTIVITY = 64;

  /** Whether the list is open: hover, tap, focus and Escape all speak to it. */
  let open = $state(false);
  /** The segment and its list, so leaving and opening can be told apart. */
  let segEl: HTMLElement | null = $state(null);
  let listEl: HTMLElement | null = $state(null);
  /** A beat of grace on leaving, so crossing the gap into the list lands. */
  let closing: ReturnType<typeof setTimeout> | null = null;

  // The COUNTS are the session's: every instance the record holds, so a
  // settled one the page cannot open still counts rather than the whole row
  // vanishing while the chat draws its dispatch. The LIST leads only where it
  // can - running instances (their row is in the live turn) and dispatches
  // the loaded conversation actually holds.
  const all = $derived(subagents.all());
  const running = $derived(all.filter((card) => card.running));
  const finished = $derived(all.length - running.length);
  const listed = $derived(transcribable(all, (id) => subagents.reachable(id)));

  /** What one instance is doing, short enough for a row: one line, capped. */
  const doing = (card: (typeof all)[number]) => {
    const said = firstLine(card.tail[card.tail.length - 1]?.title ?? 'working').trim();
    return said.length > MAX_ACTIVITY ? `${said.slice(0, MAX_ACTIVITY).trimEnd()}\u{2026}` : said;
  };

  /** The panel's measured caps, remeasured on a resize while open. */
  let limits = $state('');

  $effect(() => {
    if (!open) return;
    const remeasure = () => (limits = panelStyle(segEl));
    window.addEventListener('resize', remeasure);
    return () => window.removeEventListener('resize', remeasure);
  });

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    if (!open) limits = panelStyle(segEl);
    open = true;
  }

  /**
   * Leaving arms the close.
   *
   * **Pointer and keyboard leave differently.** A pointer leave whose event
   * carries no related target fires over the panel's own gap; if the segment
   * is still hovered it is a crossing, not a leave, and arms nothing. A
   * pointer leave may close while the TOGGLE holds focus - that element stays
   * mounted - but not while a list ROW holds it: closing would unmount the
   * row the reader is on. A blur is leaving only when focus moves outside the
   * segment at all.
   */
  function release(event?: FocusEvent) {
    if (event === undefined) {
      if (segEl !== null && segEl.matches(':hover')) return;
      const active = document.activeElement;
      const onRow =
        segEl !== null && listEl !== null && active instanceof Node && listEl.contains(active);
      if (onRow) return;
      arm();
      return;
    }
    // A blur whose related target is nothing (a click on the panel's own
    // padding) leaves focus wherever the browser put it: read the live one.
    const next = event.relatedTarget ?? document.activeElement;
    if (segEl !== null && next instanceof Node && segEl.contains(next)) return;
    arm();
  }

  function arm() {
    if (closing !== null) clearTimeout(closing);
    closing = setTimeout(() => {
      closing = null;
      open = false;
    }, 120);
  }

  /** A pointer that can hover, which a finger cannot: a tap's synthesised
   * enter must not arm the hover path, or the click that follows toggles the
   * panel straight back shut. */
  const hovering = (event: PointerEvent) => event.pointerType === 'mouse';

  /**
   * A compatibility press is in flight.
   *
   * **A tap focuses the toggle through its compat `mousedown`**, and the
   * compat sequence is what a pointer-based guard must key on: measured in
   * chromium, pointerdown and pointerup both complete BEFORE mousedown, the
   * focus and the click. Opening the list from that focus is an open the
   * tap's own click toggles shut a heartbeat later, unseen - so every tap
   * whose finger was elsewhere read as nothing. Focus opens the list only
   * while a press is in flight; `mouseup` ends every quiet press, so one
   * that focused nothing cannot shut out the next keyboard focus.
   */
  let pointed = false;

  $effect(() => {
    const press = () => (pointed = true);
    const release = () => (pointed = false);
    window.addEventListener('mousedown', press);
    window.addEventListener('mouseup', release);
    window.addEventListener('click', release);
    window.addEventListener('pointercancel', release);
    return () => {
      window.removeEventListener('mousedown', press);
      window.removeEventListener('mouseup', release);
      window.removeEventListener('click', release);
      window.removeEventListener('pointercancel', release);
    };
  });

  /** The toggle took focus: opening from it, unless a press put it there. */
  function focusIn() {
    if (pointed) {
      pointed = false;
      return;
    }
    hold();
  }

  /**
   * The list opens at its foot: the instances read oldest first, the way the
   * chat reads, so the one just started is the newest and the one a reader
   * came for. A new instance landing while the list is open is followed only
   * when the reader is already at the foot.
   */
  let seated = false;
  /** The list's height as of the previous run, so a follow judges the foot it saw. */
  let seen = 0;

  /** Put focus back on the toggle, which is where a dismissal leaves a reader. */
  function toToggle(): void {
    segEl?.querySelector('button')?.focus();
  }

  /** Put the list's scroll at its foot. */
  function toFoot(el: HTMLElement): void {
    el.scrollTop = el.scrollHeight;
  }

  /** The list's full height, read through a helper for the compiled scope. */
  function heightOf(el: HTMLElement): number {
    return el.scrollHeight;
  }

  /** Whether the foot the reader last saw is still on screen. */
  function atSeenFoot(el: HTMLElement, seen: number): boolean {
    return el.scrollTop + el.clientHeight >= seen - 48;
  }

  $effect(() => {
    const el = listEl;
    void listed.length;
    if (!open || el === null) {
      seated = false;
      return;
    }
    if (!seated) {
      seated = true;
      toFoot(el);
      seen = heightOf(el);
      return;
    }
    // Judged against the height BEFORE this run's rows landed: measured after,
    // the new content has already pushed the foot away and a batch of two or
    // more never follows.
    if (atSeenFoot(el, seen)) toFoot(el);
    seen = heightOf(el);
  });
</script>

{#if all.length > 0}
  <!-- The listeners live on the buttons themselves: the wrapper is a plain
       span, and a span wearing mouse or key handlers is a non-interactive
       element pretending to be a control. The guards in `release` are what
       keep the list open while the pointer or the keyboard is still on it. -->
  <span class="sg-seg" class:open bind:this={segEl}>
    <button
      type="button"
      class="sg-tog"
      aria-expanded={open}
      onclick={() => (open = !open)}
      onpointerenter={(event) => {
        if (hovering(event)) hold();
      }}
      onpointerleave={(event) => {
        if (hovering(event)) release();
      }}
      onfocusin={focusIn}
      onfocusout={release}
      onkeydown={(event) => {
        if (event.key !== 'Escape') return;
        // Focus first: the focusin that follows sets `open` back, so closing
        // after it is the close that sticks.
        toToggle();
        open = false;
      }}
    >
      <!-- The subagents glyph leads, so the segment reads as what it is; the
           mark after it is the state. -->
      <Icon name="subagents" />
      {#if running.length > 0}
        <span class="ring"></span>
      {:else}
        <Icon name="check" class="ok" />
      {/if}
      {running.length} running &#183; {finished} finished
    </button>

    {#if open}
      <div class="sg-list" bind:this={listEl} style={limits}>
        {#each listed as card (card.dispatch_id)}
          <button
            type="button"
            class="sg-it"
            onclick={() => {
              askReveal(card.dispatch_id);
              open = false;
            }}
            onpointerenter={(event) => {
              if (hovering(event)) hold();
            }}
            onpointerleave={(event) => {
              if (hovering(event)) release();
            }}
            onfocusout={release}
            onkeydown={(event) => {
              if (event.key !== 'Escape') return;
              toToggle();
              open = false;
            }}
          >
            {#if card.running}
              <span class="ring"></span>
            {:else if card.failed}
              <Icon name="x" class="bad" />
            {:else}
              <Icon name="check" class="ok" />
            {/if}
            <span class="nm">{card.agent_type ?? 'agent'}</span>
            <span class="tx">{card.running ? doing(card) : card.name}</span>
            {#if card.usage !== null}
              <span class="n">{duration(card.usage.duration_ms)}</span>
            {/if}
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}

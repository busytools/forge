<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { askReveal } from '../session/scroll-ask';
  import { duration } from './numbers';
  import { subagents, transcribable } from './subagents.svelte';
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

  // The counts and the list are the SAME set - the instances a transcript can
  // be opened for - so the numbers and the rows can never disagree.
  const listed = $derived(transcribable(subagents.all()));
  const running = $derived(listed.filter((card) => card.running));
  const finished = $derived(listed.length - running.length);

  /** What one instance is doing, short enough for a row: one line, capped. */
  const doing = (card: (typeof listed)[number]) => {
    const said = firstLine(card.tail[card.tail.length - 1]?.title ?? 'working').trim();
    return said.length > MAX_ACTIVITY ? `${said.slice(0, MAX_ACTIVITY).trimEnd()}\u{2026}` : said;
  };

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    open = true;
  }

  /**
   * Leaving arms the close, but focus moving WITHIN the segment is not
   * leaving it: tabbing from the toggle into the list fires a bubbling
   * `focusout` whose related target is still inside, and the close it armed
   * would unmount the element the reader just tabbed to.
   */
  function release(event?: FocusEvent) {
    // A blur whose related target is nothing (a click on the panel's own
    // padding) leaves focus wherever the browser put it: read the live one.
    const next = event?.relatedTarget ?? document.activeElement;
    if (segEl !== null && next instanceof Node && segEl.contains(next)) return;
    // A leave arms nothing while the segment still holds the reader - the
    // pointer may be crossing back in, and a row may hold the keyboard.
    if (event === undefined && segEl !== null && segEl.matches(':hover')) return;
    if (segEl !== null && document.activeElement !== null && segEl.contains(document.activeElement))
      return;
    if (closing !== null) clearTimeout(closing);
    closing = setTimeout(() => {
      closing = null;
      open = false;
    }, 120);
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

{#if listed.length > 0}
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
      onmouseenter={hold}
      onmouseleave={() => release()}
      onfocusin={hold}
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
      <div class="sg-list" bind:this={listEl}>
        {#each listed as card (card.dispatch_id)}
          <button
            type="button"
            class="sg-it"
            onclick={() => {
              askReveal(card.dispatch_id);
              open = false;
            }}
            onmouseenter={hold}
            onmouseleave={() => release()}
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

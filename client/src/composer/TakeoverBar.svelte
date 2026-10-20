<script lang="ts">
  /**
   * The browser takeover's bar: back to chat, Done, Not now, and the feedback
   * line for what did not work.
   *
   * **Drawn by the composer, outside its own container** (`.surf` is a
   * `container-type`, which makes it the containing block for fixed children),
   * so the bar sits at the window's top above the native page layer. The
   * feedback line is the dock's own words row - the same Field, TakeCard and
   * mic - so dictation here is the composer's, not a second implementation.
   */
  import Icon from '../components/Icon.svelte';
  import Field from './Field.svelte';
  import TakeCard from './TakeCard.svelte';
  import { surface } from '../browser/surface.svelte';
  import type { NavState } from '../browser/surface.svelte';
  import type { Take } from './wire';

  let {
    title,
    notes = $bindable(''),
    take = null,
    wire = null,
    silent = null,
    nav = { can_back: false, can_forward: false },
    onabandon = () => {},
    onmic = () => {},
    dictation = false,
    ondone = null,
    onnotnow = null,
    onback,
    onnav = null,
    onfeedback = null,
    onretry = null,
  }: {
    title: string;
    notes?: string;
    take?: Take | null;
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
    /** The hand-off's answers; null once no ask is parked, leaving back-to-chat. */
    ondone?: (() => void) | null;
    onnotnow?: (() => void) | null;
    onback: () => void;
    /** The history arrows' click; their keys are the surface's own. */
    onnav?: ((direction: 'back' | 'forward') => void) | null;
    onfeedback?: ((text: string) => void) | null;
    /** Present while the surface failed: the door back to a live page. */
    onretry?: (() => void) | null;
  } = $props();

  let feedbackOpen = $state(false);
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
    const text = notes.trim();
    if (text === '' || onfeedback === null) return;
    onfeedback(text);
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
        disabled={!nav.can_back}
        onclick={() => onnav?.('back')}
      >
        <Icon name="back" />
      </button>
      <button
        class="tb-btn nav"
        type="button"
        aria-label="forward"
        title="forward (Cmd+])"
        disabled={!nav.can_forward}
        onclick={() => onnav?.('forward')}
      >
        <Icon name="forward" />
      </button>
    </span>
    <span class="tb-title">{title}</span>
    {#if onretry !== null}
      <span class="tb-acts">
        <button class="tb-btn" type="button" onclick={onretry}>try again</button>
      </span>
    {/if}
    {#if ondone !== null && onnotnow !== null}
      <span class="tb-acts">
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
      </span>
    {/if}
  </div>
  {#if feedbackOpen && onfeedback !== null}
    <div class="tb-feedback">
      {#if take !== null}
        <TakeCard {take} phase={take.phase} {wire} oncancel={onabandon} {silent} spread />
      {:else}
        <Field
          editor="dock"
          class="tb-field"
          bind:value={notes}
          rows={1}
          placeholder="what did not work? the agent reads this"
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
          aria-label={take === null ? 'dictate the feedback' : 'stop dictating'}
          onclick={onmic}
        >
          <Icon name="mic" />
        </button>
      {/if}
      {#if take === null && notes.trim() !== ''}
        <button
          class="sendb"
          type="button"
          aria-label="send the feedback and decline the hand-off"
          onclick={send}
        >
          <Icon name="send" />
        </button>
      {/if}
    </div>
  {/if}
</div>

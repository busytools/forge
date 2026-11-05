<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { terminalRecord } from '../terminal/record.svelte';
  import { closeTerminal, openTerminal, surface } from '../terminal/surface.svelte';
  import type { TerminalSummary } from '../wire/terminal';
  import type { SessionSlot } from '../wire/types';

  /**
   * The terminal row in the strip above the composer: the seat's terminal and
   * what it is doing, with **show** / **hide**, and **start** on a seat that
   * has none.
   *
   * **The row is the person's door even when no terminal exists.** An agent
   * that never opened one must not leave the person with no way in, so the
   * row is drawn for every seat and its button opens one. The terminal itself
   * outlives the surface: hiding it is not closing it.
   */
  let {
    slot = null,
    terminal = null,
  }: { slot?: SessionSlot | null; terminal?: TerminalSummary | null } = $props();

  /** The seat's terminal as its record carries it, which the update keeps fresh. */
  const live = $derived(terminal ?? terminalRecord.terminal);

  /** The seat the row is for: the one named, else the record's own. */
  const seat = $derived(slot ?? terminalRecord.seat);

  /** Whether THIS page is showing the terminal. */
  const fronting = $derived(surface.phase !== 'closed' && live !== null && surface.id === live.id);

  /** Whether this page holds the raised terminal's control. */
  const mine = $derived(
    fronting && surface.viewer !== null && surface.controller === surface.viewer,
  );

  /** What the terminal is doing, in words rather than only a colour. */
  const stateText = $derived.by((): string => {
    const held = live;
    if (held === null) return 'off';
    if (!held.alive) return 'exited';
    if (held.held) return 'waiting for you';
    if (mine) return 'you have control';
    if (held.state.input_expected?.kind === 'password') return 'a password is wanted';
    // **A `prompt` is the shell's own prompt, not a person's turn**: only a
    // password or a yes/no question is the terminal waiting for the person.
    if (held.state.input_expected?.kind === 'confirm') return 'waiting for you';
    if (held.state.running) return 'a command is running';
    return 'idle';
  });

  const mark = $derived(
    live === null
      ? 'idle'
      : !live.alive
        ? 'bad'
        : live.held ||
            live.state.input_expected?.kind === 'password' ||
            live.state.input_expected?.kind === 'confirm'
          ? 'held'
          : live.state.running
            ? 'running'
            : 'idle',
  );

  async function toggle(): Promise<void> {
    const held = live;
    const here = seat;
    if (here === null) return;
    if (fronting) {
      await closeTerminal();
      return;
    }
    // **No terminal yet is the start door**: the attach names none, and the
    // server opens the seat's own.
    const why = await openTerminal(here, held?.id ?? null);
    if (why !== null) {
      // The failure is drawn by the surface's own bar; the row keeps its
      // last state rather than inventing a second sentence.
      console.warn(`the terminal did not open: ${why}`);
    }
  }
</script>

<button
  type="button"
  class="tm-tog"
  aria-pressed={fronting}
  aria-keyshortcuts="Meta+J Control+J"
  aria-label={fronting
    ? 'hide the terminal'
    : live === null
      ? 'start a terminal'
      : 'show the terminal'}
  onclick={toggle}
>
  <Icon name="square-terminal" />
  {#if mark === 'running'}
    <span class="ring" title="a command is running"></span>
  {:else if mark === 'held'}
    <span class="dot warn" title="the terminal is waiting for you"></span>
  {:else if mark === 'bad'}
    <Icon name="x" class="bad" />
  {:else}
    <span class="dot idle" title="the terminal is idle"></span>
  {/if}
  terminal
  <span class="tx">{stateText}</span>
  <span class="go">{fronting ? 'hide' : live === null ? 'start' : 'show'}</span>
</button>

<style>
  /* The browser chip's dress: one borderless pill, glyph, state dot, word,
     state and door - never a bordered chip with a second bordered button
     inside it. */
  .tm-tog {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 3px 8px;
    border: 1px solid transparent;
    border-radius: 999px;
    background: none;
    color: var(--dim);
    font: inherit;
    white-space: nowrap;
    cursor: pointer;
  }
  .tm-tog:hover,
  .tm-tog:focus-visible {
    background: var(--s3);
    color: var(--text);
  }
  .tx {
    color: var(--dim);
  }
  .go {
    color: var(--muted);
  }
  .tm-tog:hover .go,
  .tm-tog:focus-visible .go {
    color: var(--text);
  }
  .ring {
    width: 8px;
    height: 8px;
    border: 2px solid var(--accent);
    border-radius: 50%;
  }
  @media (pointer: coarse) {
    .tm-tog {
      min-height: 44px;
    }
  }
</style>

<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { terminalRecord } from '../terminal/record.svelte';
  import { closeTerminal, onState, openTerminal, surface } from '../terminal/surface.svelte';
  import { terminalSummaryFrom, type TerminalSummary } from '../wire/terminal';
  import type { SessionSlot } from '../wire/types';

  /**
   * The terminal row in the strip above the composer: the seat's terminal and
   * what it is doing, with **show** / **hide**.
   *
   * **The row exists only while the seat has a terminal.** An unopened seat
   * costs no row, exactly as a profile with no tab draws none. The terminal
   * itself outlives the surface: hiding it is not closing it.
   */
  let {
    slot = null,
    terminal = null,
  }: { slot?: SessionSlot | null; terminal?: TerminalSummary | null } = $props();

  /** What the session record carried, which a newer push can override. */
  const fromRecord = $derived(terminal ?? terminalRecord.terminal);
  /** The last pushed summary; the record clears it when it re-reads. */
  let pushed = $state<TerminalSummary | null>(null);
  const live = $derived(pushed ?? fromRecord);

  /** The seat the row is for: the one named, else the record's own. */
  const seat = $derived(slot ?? terminalRecord.seat);

  $effect(() => {
    // A fresh record read outranks the push it followed.
    void fromRecord;
    pushed = null;
  });

  // **The state push is the truth from then on**, so a command starting or a
  // hand-off parking moves the row without a re-read.
  $effect(() => {
    let stop: (() => void) | null = null;
    let mounted = true;
    const here = seat;
    if (here !== null) {
      stop = onState((state) => {
        if (!mounted) return;
        if (
          state.seat.org !== here.org ||
          state.seat.project !== here.project ||
          state.seat.label !== here.label
        ) {
          return;
        }
        pushed = terminalSummaryFrom(state.terminal);
      });
    }
    return () => {
      mounted = false;
      stop?.();
    };
  });

  /** Whether THIS page is showing the terminal. */
  const fronting = $derived(surface.phase !== 'closed' && live !== null && surface.id === live.id);

  /** What the terminal is doing, in words rather than only a colour. */
  const stateText = $derived.by((): string => {
    const held = live;
    if (held === null) return 'no terminal';
    if (!held.alive) return 'exited';
    if (held.held) return 'waiting for you';
    if (held.state.input_expected?.kind === 'password') return 'a password is wanted';
    if (held.state.input_expected !== null) return 'waiting for you';
    if (held.state.running) return 'a command is running';
    return 'idle';
  });

  const mark = $derived(
    live === null
      ? 'idle'
      : !live.alive
        ? 'bad'
        : live.held || live.state.input_expected !== null
          ? 'held'
          : live.state.running
            ? 'running'
            : 'idle',
  );

  async function toggle(): Promise<void> {
    const held = live;
    const here = seat;
    if (held === null || here === null) return;
    if (fronting) {
      await closeTerminal();
      return;
    }
    const why = await openTerminal(here, held.id);
    if (why !== null) {
      // The failure is drawn by the surface's own bar; the row keeps its
      // last state rather than inventing a second sentence.
      console.warn(`the terminal did not open: ${why}`);
    }
  }
</script>

{#if live !== null}
  <span class="tm-seg">
    <span class="tm-state">
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
    </span>
    <button
      type="button"
      class="tm-show"
      aria-pressed={fronting}
      aria-label={fronting ? 'hide the terminal' : 'show the terminal'}
      onclick={toggle}
    >
      {fronting ? 'hide' : 'show'}
    </button>
  </span>
{/if}

<style>
  .tm-seg {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 2px 8px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--s1);
    color: var(--text);
    font-size: 12.5px;
  }
  .tm-state {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
  }
  .tx {
    color: var(--muted);
  }
  .tm-show {
    min-height: 28px;
    padding: 2px 10px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: var(--s2);
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }
  .ring {
    width: 8px;
    height: 8px;
    border: 2px solid var(--accent);
    border-radius: 50%;
  }
  @media (pointer: coarse) {
    .tm-show {
      min-height: 44px;
    }
  }
</style>

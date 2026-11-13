<script module lang="ts">
  /** How many pickers this page has mounted, for history-entry marks. */
  let pickers = 0;
</script>

<script lang="ts">
  /**
   * One choice from a list, drawn by the page rather than by the platform.
   *
   * **A native `<select>` is the control this replaces.** Its chooser is the
   * OS's own - unstyleable, and on Android its arrow sits tight against the
   * text - so the trigger here is the page's button with the house chevron,
   * and the list is a bottom sheet the page draws (issue #2968).
   *
   * **The sheet is an overlay like every other**: it takes a history entry so
   * the hardware Back closes it, it closes on Escape, it keeps Tab inside
   * itself, and the keyboard goes back to the trigger when it closes.
   *
   * It reads the theme's tokens only, so the next page that needs a choice
   * takes this component rather than copying it (rule 21).
   */
  import { onDestroy } from 'svelte';

  import Chevron from './Chevron.svelte';
  import Icon from './Icon.svelte';
  import type { PickOption } from './picker';

  let {
    label,
    options,
    value,
    onchange,
    title = 'choose one',
    disabled = false,
  }: {
    /** The visible label beside the trigger; the trigger's name adds the seat. */
    label: string;
    options: PickOption[];
    value: string | null;
    onchange: (next: string) => void;
    /** The sheet's heading. */
    title?: string;
    /** Whether the choice is frozen - an upload or a take already under way. */
    disabled?: boolean;
  } = $props();

  /** This instance's history-entry mark, so two pickers cannot close each other. */
  const entry = `forgePicker${String(pickers++)}`;

  let open = $state(false);
  let trigger = $state<HTMLButtonElement | null>(null);
  let sheet = $state<HTMLElement | null>(null);

  const current = $derived(options.find((option) => option.value === value) ?? null);
  /** What the trigger says it is: the field's label and the seat it holds. */
  const name = $derived(`${label}: ${current?.label ?? 'none'}`);

  /** The current entry's state, as an object this component can read. */
  function heldState(): Record<string, unknown> {
    const held: unknown = history.state;
    return typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
  }

  function openSheet(): void {
    if (disabled || open) return;
    history.pushState({ ...heldState(), [entry]: true }, '', location.href);
    open = true;
  }

  /** Close through the entry when it is on top, so Back and this are one path. */
  function close(): void {
    if (heldState()[entry] === true) {
      history.back();
      return;
    }
    closed();
  }

  /** The close itself: no history, the state and the keyboard. */
  function closed(): void {
    open = false;
    trigger?.focus();
  }

  function choose(next: string): void {
    onchange(next);
    close();
  }

  // **A remount strands nothing.** A second share replaces the page under an
  // open sheet; this component's popstate listener goes with it, and the entry
  // it took would swallow the next Back. Pop that entry on the way out.
  onDestroy(() => {
    if (open && heldState()[entry] === true) history.back();
  });

  // The hardware Back, and every other move through history, lands here.
  $effect(() => {
    const restore = (): void => {
      if (open) closed();
    };
    addEventListener('popstate', restore);
    return () => removeEventListener('popstate', restore);
  });

  // **The keyboard starts on the answer already given**, or on the first row
  // when nothing is picked, so a reader can press Enter twice without
  // hunting. Read through `sheet`, so the run that first sees the bound
  // element is the one that places the keyboard.
  $effect(() => {
    if (!open || sheet === null) return;
    const marked = sheet.querySelector<HTMLElement>('.pickopt[aria-current="true"]');
    const first = sheet.querySelector<HTMLElement>('.pickopt');
    (marked ?? first)?.focus();
  });

  function key(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.preventDefault();
      close();
      return;
    }
    if (event.key !== 'Tab' || sheet === null) return;
    const focusable = [
      ...sheet.querySelectorAll<HTMLElement>('button, [href], input, select, textarea'),
    ].filter((element) => !element.hasAttribute('disabled'));
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (first === undefined || last === undefined) return;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }
</script>

<div class="picker">
  <span class="lab">{label}</span>
  <button
    class="pick"
    type="button"
    bind:this={trigger}
    {disabled}
    aria-haspopup="dialog"
    aria-expanded={open}
    aria-label={name}
    onclick={openSheet}
  >
    <span class="who">{current?.label ?? 'none'}</span>
    <Chevron />
  </button>
</div>

{#if open}
  <!-- **Not `.scrim`**: that class is the session rail's, and its global rule
       keeps it invisible and off the pointer until the rail opens. -->
  <div class="veil" role="presentation" onclick={close}></div>
  <div
    class="sheet"
    role="dialog"
    aria-modal="true"
    aria-label={title}
    tabindex="-1"
    bind:this={sheet}
    onkeydown={key}
  >
    <div class="grab" aria-hidden="true"></div>
    <h2>{title}</h2>
    <!-- **The rows are the part that scrolls.** The sheet is bounded by the
         viewport, with the grab bar, the title and cancel pinned, so a roster
         longer than the screen keeps its way out and its heading instead of
         pushing them off the top (the dock's `.opts` is the same shape). -->
    <div class="opts">
      {#each options as option (option.value)}
        <button
          class="pickopt"
          class:current={option.value === value}
          type="button"
          aria-current={option.value === value ? 'true' : undefined}
          onclick={() => choose(option.value)}
        >
          <span class="who">{option.label}</span>
          {#if option.note !== undefined}<span class="note">{option.note}</span>{/if}
          {#if option.value === value}<Icon
              name="check"
              class="tick"
              label="the current choice"
            />{/if}
        </button>
      {/each}
    </div>
    <button class="cancel" type="button" onclick={close}>cancel</button>
  </div>
{/if}

<style>
  .picker {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .lab {
    color: var(--dim);
    font-family: var(--mono);
    font-size: 12.5px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .pick {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    min-height: 44px;
    padding: 0 14px 0 14px;
    border: 1px solid var(--ctl);
    border-radius: 9px;
    background: var(--s2);
    color: var(--text);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }

  .pick[disabled] {
    color: var(--dim);
    cursor: default;
  }

  .pick .who {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* The house chevron, turned to point down and never squeezed: the room the
     native select's arrow on Android did not have. **`opacity: 1` because the
     shared `.arw` is a disclosure arrow that starts hidden**, shown only under
     a `<details>` summary; without it this trigger draws no chevron at all. */
  .pick :global(.arw) {
    flex: none;
    opacity: 1;
    transform: rotate(90deg);
    color: var(--muted);
    margin-right: 6px;
  }

  .veil {
    position: fixed;
    inset: 0;
    z-index: 30;
    /* The theme's own ground, not a palette value of its own (rule 21). */
    background: color-mix(in srgb, var(--bg) 62%, transparent);
  }

  .sheet {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 31;
    display: flex;
    flex-direction: column;
    max-height: min(78dvh, 640px);
    background: var(--s2);
    border-top: 1px solid var(--ctl);
    border-radius: 18px 18px 0 0;
    padding: 10px 0 calc(16px + var(--safe-bottom));
    box-shadow: 0 -18px 50px rgba(0, 0, 0, 0.55);
  }

  .grab {
    flex: none;
    width: 42px;
    height: 4px;
    border-radius: 2px;
    background: var(--ctl);
    margin: 0 auto 10px;
  }

  .sheet h2 {
    flex: none;
    margin: 0;
    padding: 4px 18px 10px;
    font-size: 13px;
    font-weight: 500;
    color: var(--muted);
    font-family: var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .opts {
    flex: 0 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    overscroll-behavior: contain;
  }

  /* **Not `.opt`**: that class is the dock's, and its global rule would round
     every row and the current row's band with the dock's own radius. */
  .pickopt {
    display: flex;
    flex: none;
    align-items: center;
    gap: 12px;
    min-height: 48px;
    padding: 0 18px;
    border: 0;
    background: transparent;
    color: var(--text);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }

  .pickopt.current {
    background: var(--sel);
  }

  .pickopt .who {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .pickopt .note {
    color: var(--dim);
    font-family: var(--mono);
    font-size: 11.5px;
  }

  .pickopt :global(.tick) {
    flex: none;
    color: var(--accent);
  }

  .cancel {
    flex: none;
    min-height: 44px;
    margin: 10px 18px 0;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: transparent;
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }
</style>

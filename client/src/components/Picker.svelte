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
    /** The visible label beside the trigger, and the trigger's accessible name. */
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
    const marked = sheet.querySelector<HTMLElement>('.opt[aria-current="true"]');
    const first = sheet.querySelector<HTMLElement>('.opt');
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
    aria-label={label}
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
    {#each options as option (option.value)}
      <button
        class="opt"
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
    background: rgba(2, 3, 7, 0.62);
  }

  .sheet {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 31;
    display: flex;
    flex-direction: column;
    background: var(--s2);
    border-top: 1px solid var(--ctl);
    border-radius: 18px 18px 0 0;
    padding: 10px 0 16px;
    box-shadow: 0 -18px 50px rgba(0, 0, 0, 0.55);
  }

  .grab {
    width: 42px;
    height: 4px;
    border-radius: 2px;
    background: var(--ctl);
    margin: 0 auto 10px;
  }

  .sheet h2 {
    margin: 0;
    padding: 4px 18px 10px;
    font-size: 13px;
    font-weight: 500;
    color: var(--muted);
    font-family: var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .opt {
    display: flex;
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

  .opt.current {
    background: var(--sel);
  }

  .opt .who {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .opt .note {
    color: var(--dim);
    font-family: var(--mono);
    font-size: 11.5px;
  }

  .opt :global(.tick) {
    flex: none;
    color: var(--accent);
  }

  .cancel {
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

<script lang="ts">
  /**
   * A styled picker: a button, and the list it opens.
   *
   * A native `select` hands its dropdown to the platform - a menu in another
   * font, in another shape, that no stylesheet reaches - so the board draws
   * its own. Keyboard: the button opens on Enter, Space or ArrowDown; while
   * open, the arrows move and Enter picks; Escape closes, as does a click
   * outside.
   */
  let {
    label,
    value,
    options,
    onpick,
  }: {
    label: string;
    value: string;
    options: { value: string; label: string }[];
    onpick: (value: string) => void;
  } = $props();

  let open = $state(false);
  let active = $state(0);
  let root: HTMLElement | undefined = $state();

  const shown = $derived(options.find((option) => option.value === value)?.label ?? value);

  function toggle(): void {
    open = !open;
    if (open)
      active = Math.max(
        0,
        options.findIndex((option) => option.value === value),
      );
  }

  function pick(option: { value: string }): void {
    onpick(option.value);
    open = false;
  }

  /** Close when the click lands outside this component. */
  function outside(event: MouseEvent): void {
    // The event's target is an EventTarget; only an Element can be inside us.
    const target = event.target;
    if (open && root !== undefined && target instanceof Node && !root.contains(target)) {
      open = false;
    }
  }

  function key(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      open = false;
      return;
    }
    if (!open) {
      if (event.key === 'Enter' || event.key === ' ' || event.key === 'ArrowDown') {
        event.preventDefault();
        toggle();
      }
      return;
    }
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      active = Math.min(options.length - 1, active + 1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      active = Math.max(0, active - 1);
    } else if (event.key === 'Enter') {
      const option = options[active];
      if (option !== undefined) pick(option);
    }
  }
</script>

<svelte:window onclick={outside} />

<div class="b-drop" bind:this={root}>
  <button
    type="button"
    class="b-pick"
    aria-label={label}
    aria-haspopup="listbox"
    aria-expanded={open}
    onclick={toggle}
    onkeydown={key}
  >
    <span class="b-pick-t">{shown}</span>
    <svg
      class="b-chev"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      aria-hidden="true"><path d="M6 9l6 6 6-6" /></svg
    >
  </button>
  {#if open}
    <!-- A listbox's children have to BE its options: a `li` between them is
         a node the role does not allow, which axe reads as a broken menu.
         So the options are the buttons themselves, directly. -->
    <div class="b-menu" role="listbox" aria-label={label}>
      {#each options as option, i (option.value)}
        <button
          type="button"
          role="option"
          aria-selected={option.value === value}
          class="b-item"
          class:on={option.value === value}
          class:hot={i === active}
          onclick={() => pick(option)}
          onmousemove={() => (active = i)}
        >
          {option.label}
        </button>
      {/each}
    </div>
  {/if}
</div>

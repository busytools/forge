<script lang="ts">
  /**
   * The dictation panel: what a take is set to, in the web's own form.
   *
   * Not a port of the terminal's `/dictate` overlay - the same data, drawn as a
   * panel. Each axis is a short exclusive set, so each is a row of chips rather
   * than a list of rows: the whole panel reads at a glance, every option is one
   * click, and nothing is hidden behind a submenu. The in-force chip is filled
   * AND outlined as well as coloured, so the state is not carried by hue alone.
   *
   * The axes and the input are the CLIENT's since capture moved here: they are
   * held by the page that owns them (per seat, remembered on this machine),
   * and the panel is handed the values in force plus the two setters. The
   * defaults the greeting carried are what a deviation is marked against and
   * what the reset row returns to.
   *
   * The MODE is stated rather than offered, because the code has nothing to
   * honour a click with: it is `forge.toml`'s, and a mode chip would be a
   * control nothing reads.
   */
  import Icon from '../components/Icon.svelte';
  import type { DictateAxes } from '../session/wire';
  import type { Bind, Mode } from './dictate-key';
  import {
    DESTINATION,
    keyHint,
    MODES,
    OVERRIDE,
    STRUCTURE,
    VOICE,
    type Axis,
    type Option,
  } from './dictation';
  import { inputLine, inputs, type Input } from './mic';

  let {
    axes,
    defaults,
    bind,
    mode,
    device = null,
    onaxes,
    ondevice,
  }: {
    /** The axes in force for this seat, which this client holds. */
    axes: DictateAxes;
    /** What `forge.toml` set, which a deviation is marked against. */
    defaults: DictateAxes;
    /** The push-to-talk key, whose hint the header carries. */
    bind: Bind;
    /** How a press maps onto a take, which is stated rather than offered. */
    mode: Mode;
    /** The input this seat records from, or `null` for the system default. */
    device?: string | null;
    onaxes: (axes: DictateAxes) => void;
    ondevice: (device: string | null) => void;
  } = $props();

  /** Whether this platform delivers Cmd, which is what the hint names. */
  const mac = navigator.platform.toLowerCase().includes('mac');
  const hint = $derived(keyHint(bind, mac));

  /** The value under the take's own rule, per mode, which the row prints. */
  const RULES: Readonly<Record<Mode, string>> = {
    auto: 'a quick tap toggles · a hold transcribes on release',
    toggle: 'a press starts · the next press stops',
    hold: 'hold records · release transcribes',
  };

  /** The axes a chip may set, each with the title it is drawn under. */
  const AXES: readonly { key: Axis; title: string; options: readonly Option<string>[] }[] = [
    { key: 'voice', title: 'VOICE', options: VOICE },
    { key: 'structure', title: 'STRUCTURE', options: STRUCTURE },
    { key: 'destination', title: 'DESTINATION', options: DESTINATION },
  ];

  /** The override field an axis reads, which is the core's own name for it. */
  function field(axis: Axis): 'styling' | 'structure' | 'context' {
    return OVERRIDE[axis] as 'styling' | 'structure' | 'context';
  }

  /** What this client set on an axis, which is what the tag marks. */
  function sessionSet(axis: Axis): boolean {
    return axes[field(axis)] !== defaults[field(axis)];
  }

  /** The value in force on an axis, which is what its chips compare against. */
  function inForceOn(axis: Axis): string {
    return axes[field(axis)];
  }

  function set(axis: Axis, value: string): void {
    onaxes({ ...axes, [field(axis)]: value });
  }

  function reset(): void {
    onaxes(defaults);
    ondevice(null);
  }

  /**
   * The inputs this machine offers, asked for when the list is opened.
   *
   * On demand rather than on mount: the walk asks the browser for its devices,
   * so a page that never opens the list never asks. The browser's LABELS need
   * the microphone granted once in this origin, so a fresh install lists
   * unnamed rows until a take has been allowed.
   */
  let devices = $state<Input[] | null>(null);
  let listOpen = $state(false);
  /** Whether a walk is in flight, so the row says it is looking. */
  let walking = $state(false);
  /** Why a walk failed, which the list region draws where the list would be. */
  let refused = $state<string | null>(null);

  /**
   * Show the list, or put it away again.
   *
   * The walk runs ONCE: a click while one is in flight must not start another,
   * and a list already walked is not walked again. That is what makes clicking
   * the row a toggle rather than a queue of walks.
   */
  async function openList(): Promise<void> {
    listOpen = !listOpen;
    if (!listOpen || devices !== null || walking) return;
    walking = true;
    refused = null;
    try {
      devices = await inputs();
    } catch (why) {
      refused = inputLine(why);
    } finally {
      walking = false;
    }
  }

  /** The input in force, as the row names it. */
  const inForceDevice = $derived.by(() => {
    if (device === null) return 'System default';
    return devices?.find((held) => held.id === device)?.label || 'System default';
  });

  /**
   * Whether the input in force is one the walk could not find.
   *
   * A device can be unplugged between takes, and the row tags that rather than
   * only failing at record time - the id is named and marked instead of reading
   * as an input that is present.
   */
  const missingPick = $derived(
    device !== null && devices !== null && !devices.some((held) => held.id === device),
  );

  function choose(pick: string | null): void {
    listOpen = false;
    ondevice(pick);
  }

  /** The panel's own box, whose top edge the cap is measured from. */
  let pop = $state<HTMLDivElement | null>(null);
  /** How tall the panel may be, in pixels, or `null` before it is measured. */
  let room = $state<number | null>(null);

  /**
   * Cap the panel by the room above the box rather than by a share of the
   * viewport.
   *
   * The page is a fixed-height element that does not scroll, so a panel taller
   * than the room above it has its top clipped with no way to reach it - the
   * title and the key hint go first, and nothing can scroll them back. Anchored
   * here, the panel's own bottom edge IS the box's top edge, so the room is
   * that offset; below the box is past the fold, which is why this opens upward
   * and caps rather than flipping. The cap only ever shrinks the panel, and the
   * panel scrolls inside it, so every row stays reachable.
   */
  $effect(() => {
    const held = pop;
    if (held === null) return;
    const measure = (): void => {
      // No floor: a floor is what reintroduces the clipped top, and a panel
      // that shrinks to the room it has is always reachable while one whose
      // header is cut off is not.
      room = Math.max(0, held.getBoundingClientRect().bottom - 6);
    };
    measure();
    window.addEventListener('resize', measure);
    // **The window is not the only thing that moves the box.** A list opening
    // above the field, or a take row landing above it, grows the composer and
    // lifts the panel with it - and a cap measured on mount would stay where it
    // was and cut the panel's own top off, which is the failure this cap
    // exists to prevent. So the composer's own geometry is watched too, and
    // what a SIZE observer does not catch is a pure move: an ancestor shifting
    // the box and the panel together without changing either one's size. The
    // window listener is what covers that, and neither signal alone is enough.
    const watching = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    watching?.observe(held.parentElement ?? held);
    return () => {
      window.removeEventListener('resize', measure);
      watching?.disconnect();
    };
  });
</script>

<div
  class="pop"
  role="dialog"
  aria-label="Dictation"
  bind:this={pop}
  style={room === null ? undefined : `max-height: ${room}px`}
>
  <div class="hd">
    <span class="t">Dictation</span>
    {#if hint !== null}
      <span class="k"><b>{hint}</b></span>
    {/if}
  </div>
  <div class="scope">axes this session &#183; input on this machine</div>

  {#each AXES as axis (axis.key)}
    <div class="ax">
      <div class="lbl">
        {axis.title}
        {#if sessionSet(axis.key)}<span class="src">&#183; this session</span>{/if}
      </div>
      <div class="chips">
        {#each axis.options as option (option.value)}
          <button
            class="chip"
            class:on={inForceOn(axis.key) === option.value}
            type="button"
            aria-pressed={inForceOn(axis.key) === option.value}
            onclick={() => set(axis.key, option.value)}
          >
            {option.label}
          </button>
        {/each}
      </div>
    </div>
  {/each}

  <div class="ax">
    <div class="lbl">MODE <span class="src">&#183; from forge.toml</span></div>
    <div class="chips">
      {#each MODES as option (option.value)}
        <span class="chip" class:on={mode === option.value} aria-current={mode === option.value}>
          {option.label}
        </span>
      {/each}
    </div>
    <div class="note">{RULES[mode]}</div>
  </div>

  <div class="ax">
    <div class="lbl">INPUT DEVICE</div>
    <button
      class="dev"
      class:missing={missingPick}
      type="button"
      aria-expanded={listOpen}
      onclick={openList}
    >
      <Icon name="mic" />
      <span class="nm">{inForceDevice}</span>
      {#if missingPick}
        <span class="tag">not present</span>
      {/if}
      <span class="chev">&#9656;</span>
    </button>
    {#if listOpen}
      <div class="list" aria-label="Input device">
        {#if walking}
          <div class="note">Looking for inputs...</div>
        {:else if refused !== null}
          <div class="note bad">{refused}</div>
        {:else}
          <button class="row" type="button" onclick={() => choose(null)}>
            <span class="nm">System default</span>
          </button>
          {#each devices ?? [] as held, at (held.id)}
            <button class="row" type="button" onclick={() => choose(held.id)}>
              <span class="nm">{held.label || `Microphone ${at + 1}`}</span>
            </button>
          {/each}
          {#if devices !== null && devices.length === 0}
            <div class="note">No input devices found.</div>
          {/if}
        {/if}
      </div>
    {/if}
    <div class="note">
      the browser's own list &#183; names appear once this page has been allowed the microphone
    </div>
  </div>

  <div class="ft">
    <button class="rst" type="button" onclick={reset}>Reset all to defaults</button>
  </div>
</div>

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
   * Two things it states rather than offers, because the code has nothing to
   * honour a click with: the MODE, which the terminal keeps in `forge.toml`
   * too, and the DEVICE, whose pick lives on the home snapshot and outlives
   * this page. Both say where they come from, which is what the device's own
   * caveat asks for.
   */
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { DictateDevice } from '../protocol';
  import type { DictateOverrides } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import type { Bind, Mode } from './dictate-key';
  import {
    DESTINATION,
    inForce,
    keyHint,
    MODES,
    OVERRIDE,
    pickUpdate,
    STRUCTURE,
    VOICE,
    type Axis,
    type Option,
  } from './dictation';

  let {
    overrides,
    bind,
    mode,
    slot,
    connection,
    device = null,
  }: {
    overrides: DictateOverrides;
    /** The push-to-talk key, whose hint the header carries. */
    bind: Bind;
    /** How a press maps onto a take, which is stated rather than offered. */
    mode: Mode;
    slot: SessionSlot;
    connection: Pick<Connection, 'dispatch' | 'onMessage' | 'devices'>;
    /** The input a pick moved the process to, or `null` while the pin stands. */
    device?: { device: string } | 'system' | null;
  } = $props();

  /** Whether this platform delivers Cmd, which is what the hint names. */
  const mac = navigator.platform.toLowerCase().includes('mac');
  const hint = $derived(keyHint(bind, mac));
  const force = $derived(inForce(overrides));

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

  /** What the session itself set on an axis, which is what the tag marks. */
  function sessionSet(axis: Axis): boolean {
    return overrides[field(axis)] !== null;
  }

  /** The value in force on an axis, which is what its chips compare against. */
  function inForceOn(axis: Axis): string {
    return force[field(axis)];
  }

  function set(axis: Axis, value: string): void {
    void connection.dispatch({
      set_dictate_override: { key: slot, update: pickUpdate(axis, value) },
    });
  }

  function reset(): void {
    void connection.dispatch({ reset_dictate_overrides: { key: slot } });
  }

  /**
   * The inputs forge can record from, asked for when the list is opened.
   *
   * On demand rather than with the record: the walk opens the microphone stack,
   * so a page that never opens the list never trips it.
   */
  let devices = $state<DictateDevice[] | null>(null);
  let configured = $state<string | null>(null);
  let listOpen = $state(false);
  let walked = $state(false);
  /** Whether a walk is in flight, so the row says it is looking. */
  let walking = $state(false);
  /** Why a walk failed, which the list region draws where the list would be. */
  let refused = $state<string | null>(null);

  $effect(() => {
    return connection.onMessage((message) => {
      if (message.kind === 'devices') {
        devices = message.devices;
        configured = message.configured;
        walked = true;
        walking = false;
        refused = null;
        listOpen = true;
        return;
      }
      // The socket's contract is explicit: a walk that could not enumerate
      // comes back as an error naming `devices`, and a client renders it where
      // the list would have been. Drawing nothing would make a failed walk read
      // exactly like a walk that found no inputs.
      if (message.kind === 'error' && message.what === 'devices') {
        refused = message.why;
        walking = false;
        listOpen = true;
      }
    });
  });

  /**
   * Show the list, or put it away again.
   *
   * The walk is asked for ONCE: it opens the microphone stack, so a click while
   * one is in flight must not start another, and a list already walked is not
   * walked again. That is what makes clicking the row a toggle rather than a
   * queue of walks.
   */
  function openList(): void {
    listOpen = !listOpen;
    if (!listOpen || walked || walking) return;
    walking = true;
    refused = null;
    // A socket that is not open answers `false`, and the row then keeps drawing
    // what it already knows rather than promising a list that is not coming.
    if (!connection.devices()) walking = false;
  }

  /** The device in force: the process pick, else the pin, else the system's own. */
  const inForceDevice = $derived.by(() => {
    if (device === 'system') return 'System default';
    const picked = typeof device === 'object' && device !== null ? device.device : null;
    const wanted = picked ?? configured;
    if (wanted === null) return 'System default';
    return devices?.find((held) => held.id === wanted)?.name ?? wanted;
  });

  /** The device a pick moved the process to, or `null` while the pin stands. */
  const picked = $derived(typeof device === 'object' && device !== null ? device.device : null);
  /** Whether the reader picked the system default, which overrides the pin. */
  const onSystem = $derived(device === 'system');
  /** Whether the walk found every id it was given. */
  const found = (id: string | null): boolean =>
    id === null || (devices ?? []).some((held) => held.id === id);

  /**
   * Whether the input in force is one the walk could not find - a PICK or the
   * config's own PIN, which the terminal words differently.
   *
   * A pin can name a device that is not plugged in, and the terminal tags that
   * row rather than only failing at record time - so the id is named and marked
   * instead of reading as an input that is present. A pick that is gone is the
   * reader's own doing and reads as `not present`; the pin is the config's, so
   * it says where it came from.
   */
  const missingPick = $derived(devices !== null && picked !== null && !found(picked));
  const missingPin = $derived(
    devices !== null && !onSystem && picked === null && configured !== null && !found(configured),
  );

  function choose(pick: 'system' | { device: string }): void {
    listOpen = false;
    void connection.dispatch({ set_dictate_device: { key: slot, pick } });
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
  <div class="scope">axes this session &#183; device until restart</div>

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
      class:missing={missingPick || missingPin}
      type="button"
      aria-expanded={listOpen}
      onclick={openList}
    >
      <Icon name="mic" />
      <span class="nm">{inForceDevice}</span>
      {#if missingPin}
        <span class="tag">not present &#183; pinned in forge.toml</span>
      {:else if missingPick}
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
          <button class="row" type="button" onclick={() => choose('system')}>
            <span class="nm">System default</span>
          </button>
          {#each devices ?? [] as held (held.id)}
            <button class="row" type="button" onclick={() => choose({ device: held.id })}>
              <span class="nm">{held.name}</span>
              {#if held.is_default}<span class="def">system picks it</span>{/if}
            </button>
          {/each}
          {#if walked && (devices ?? []).length === 0}
            <div class="note">No input devices found.</div>
          {/if}
        {/if}
      </div>
    {/if}
    <div class="note">
      from <span class="mono">forge.toml</span> &#183; a pick here reverts on restart, and the config's
      own device comes back
    </div>
  </div>

  <div class="ft">
    <button class="rst" type="button" onclick={reset}>Reset all to defaults</button>
  </div>
</div>

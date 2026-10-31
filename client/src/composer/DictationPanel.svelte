<script lang="ts">
  /**
   * The dictation panel: what a take is set to, in the web's own form.
   *
   * Not a port of the `/dictate` overlay in the view this client replaced - the
   * same data, drawn as a panel. Each axis is a short exclusive set, so each is
   * a row of chips rather than a list of rows: the whole panel reads at a
   * glance, every option is one click, and nothing is hidden behind a submenu.
   * The in-force chip is filled AND outlined as well as coloured, so the state
   * is not carried by hue alone.
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
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { watchModels } from '../models/live';
  import { roleModels, roleWord } from '../models/view';
  import type { DictateModelsWire, ModelRole } from '../wire/models';
  import type { ComposerConnection } from './view';
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
  import { defaultDeviceFor } from './dictation';
  import { inputLine, inputs, type Input } from './mic';

  let {
    connection = null,
    axes,
    defaults,
    bind,
    mode,
    device = null,
    anchor,
    onaxes,
    ondevice,
    onclose,
  }: {
    /** The connection the models read and the switch ride on, when there is
     * one: a panel without it draws the axes and the input alone. */
    connection?: ComposerConnection | null;
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
    /**
     * The row the panel hangs above, when the surface draws lanes above it.
     *
     * The composer keeps the running turn's strip in that lane, and its height
     * moves with every turn; a panel anchored to the surface would move and
     * re-cap at each one (#2524). `undefined` hangs the panel off the element
     * that holds it, which is what every other surface draws - its own card.
     * `null` is a row that is off screen right now: the panel keeps the place
     * it had rather than jumping to another.
     */
    anchor?: HTMLElement | null;
    onaxes: (axes: DictateAxes) => void;
    ondevice: (device: string | null) => void;
    /** The visible way out, which a touch screen needs: a phone has no Escape. */
    onclose: () => void;
  } = $props();

  /**
   * The models in use, read from the core like the models page reads them.
   *
   * The panel draws which model each role runs and lets a press switch it -
   * the same load the models page dispatches - unless `forge.toml` sets the
   * role, where the row states the key instead: the core refuses that
   * dispatch, and a control that is always refused reads as broken.
   */
  let snapshot = $state<DictateModelsWire | null>(null);
  /** Which role's list is open, when one is. */
  let modelsOpen = $state<ModelRole | null>(null);

  /**
   * The models subscription, held for as long as the connection is.
   *
   * **Not inside the effect's own cleanup.** The effect re-runs whenever the
   * page hands this component its props again - a record frame, a new turn -
   * even though the connection is the same object, and a subscription torn
   * down there closes the store with its last reader: the subscribe that
   * follows finds no snapshot, asks the server, and the panel draws "reading
   * the models this session runs..." for the round trip, a one-frame flicker
   * of its rows on every update (#2524). So the effect notices only a
   * DIFFERENT connection, and the panel's own teardown lets the last one go.
   */
  let held: { connection: ComposerConnection; stop: () => void } | null = null;

  $effect(() => {
    const open = connection;
    if (open === null) return;
    if (held !== null && held.connection === open) return;
    held?.stop();
    held = {
      connection: open,
      stop: watchModels(open).subscribe((read) => {
        snapshot = read.wire;
      }),
    };
  });

  $effect(() => () => {
    held?.stop();
    held = null;
  });

  function switchTo(role: ModelRole, file: string): void {
    const open = connection;
    modelsOpen = null;
    if (open === null || snapshot === null) return;
    try {
      void open.dispatch({ dictate_activate: { role, file } });
    } catch {
      // A closed socket: the read the panel holds stands, and the next open
      // re-reads it.
    }
    open.refresh('dictate_models');
  }

  /** One role's models, from the read the panel holds. */
  function roleOf(role: ModelRole) {
    const wire = snapshot;
    if (wire === null) return null;
    return roleModels(role, wire.in_use, wire.installed, wire.rows);
  }

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

  /**
   * What the system default IS, once a take has opened it: the take reports
   * the input a plain stream resolved to, on either leg, and nothing else
   * names it. `null` before the first default take, when the row stays plain.
   */
  const defaultName = defaultDeviceFor()?.label ?? null;

  /** The input in force, as the row names it. */
  const inForceDevice = $derived.by(() => {
    if (device === null) return defaultRow();
    return devices?.find((held) => held.id === device)?.label || 'System default';
  });

  /** The system-default row's own words: the name, when one was learned. */
  function defaultRow(): string {
    return defaultName === null ? 'System default' : `${defaultName} (system default)`;
  }

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

  /** The panel's own box, whose place the measurement writes. */
  let pop = $state<HTMLDivElement | null>(null);
  /** The panel's distance above the host's bottom edge, or `null` before it is measured. */
  let lift = $state<number | null>(null);
  /** How tall the panel may be, in pixels, or `null` before it is measured. */
  let room = $state<number | null>(null);

  /**
   * What the panel's place is measured against, held the way the models
   * subscription is: one observation per row, let go with the panel.
   */
  let place: { above: HTMLElement; host: HTMLElement; stop: () => void } | null = null;

  /**
   * Hang the panel above the row it belongs to, and cap it by the room there.
   *
   * The page is a fixed-height element that does not scroll, so a panel taller
   * than the room above it has its top clipped with no way to reach it - the
   * title and the key hint go first, and nothing can scroll them back. The
   * panel's own bottom edge IS the row's top edge, so the room is that offset;
   * below the row is past the fold, which is why this opens upward and caps
   * rather than flipping. The cap only ever shrinks the panel, and the panel
   * scrolls inside it, so every row stays reachable.
   *
   * **The place is held from the HOST's bottom edge, not its top.** The host is
   * the surface the panel is drawn in, and on the composer that surface carries
   * the running turn's strip above the input row - a lane whose height moves
   * with every turn. Held from the bottom, a strip growing upward leaves the
   * panel where it is; held from the top it would ride the strip, and the CSS
   * anchor alone put it a frame at a negative top at every turn start (#2524).
   * The row is watched rather than the host so that what the reader grows - a
   * second row for the keys, a take card, a list - still lifts the panel, and
   * only a DIFFERENT row re-measures: the page hands this component its props
   * again on every frame, and a measure per frame is a layout per frame.
   */
  $effect(() => {
    const panel = pop;
    if (panel === null) return;
    const host = panel.parentElement;
    if (host === null) return;
    // `undefined` is a surface with no lane above its row: the host IS the row.
    const above = anchor === undefined ? host : anchor;
    // **A row that is not on screen has no geometry to measure.** The composer
    // swaps its box for the dock or a blocker, and the row the observation is
    // on is then detached: a ResizeObserver fires on it once more at 0x0, and
    // measuring that wrote `bottom: hostBottom + 6; max-height: 0` - the open
    // panel collapsed under a prompt that had nothing to do with it. So the
    // observation stops when the row leaves, the place measured last stands,
    // and the row that comes back is a new element that re-measures.
    if (above === null || !above.isConnected) {
      place?.stop();
      place = null;
      return;
    }
    if (place !== null && place.above === above && place.host === host) return;
    place?.stop();
    const measure = (): void => {
      const box = host.getBoundingClientRect();
      // **The host's PADDING box is the containing block the CSS anchor used**,
      // so a bordered host (the review's thread card) must hang the panel off
      // `clientHeight`, not the border-box rect: the two differ by the border.
      const hostTop = box.top + host.clientTop;
      const hostBottom = hostTop + host.clientHeight;
      // The row is the host itself on every surface without a lane above it.
      const rowTop = above === host ? hostTop : above.getBoundingClientRect().top;
      lift = Math.max(0, hostBottom - rowTop + 6);
      // No floor beyond zero: a floor is what reintroduces the clipped top, and
      // a panel that shrinks to the room it has is always reachable while one
      // whose header is cut off is not.
      room = Math.max(0, rowTop - 12);
    };
    measure();
    // The window is the other thing that moves the row: a resize, a rotation.
    window.addEventListener('resize', measure);
    const watching = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure);
    watching?.observe(above);
    place = {
      above,
      host,
      stop: () => {
        window.removeEventListener('resize', measure);
        watching?.disconnect();
      },
    };
  });

  $effect(() => () => {
    place?.stop();
    place = null;
  });
</script>

<div
  class="pop"
  role="dialog"
  aria-label="Dictation"
  bind:this={pop}
  style={room === null || lift === null ? undefined : `bottom: ${lift}px; max-height: ${room}px`}
>
  <div class="hd">
    <span class="t">Dictation</span>
    {#if hint !== null}
      <span class="k"><b>{hint}</b></span>
    {/if}
    <button class="x" type="button" aria-label="close dictation settings" onclick={onclose}>
      <Icon name="x" />
    </button>
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

  {#if snapshot !== null}
    <!-- The models each role runs, in the panel that already says what a
         take is set to: the same read the models page draws, and the same
         activation its rows dispatch. A role `forge.toml` sets is STATED -
         the chip and the key - because the core refuses the load. -->
    {#each ['transcribing', 'normalization'] as const as role (role)}
      {@const held = roleOf(role)}
      {#if held !== null && held.current !== null}
        <div class="ax">
          <div class="lbl">
            {roleWord(role).toUpperCase()}
            {#if held.pinned}<span class="src">&#183; from forge.toml</span>{/if}
          </div>
          {#if held.pinned}
            <div class="chips">
              <span class="chip on" aria-current="true">{held.current.file}</span>
            </div>
            <div class="note">
              set by <code>[dictate] {held.pinKey}</code> &middot; remove the key to change it here
            </div>
          {:else}
            <button
              class="dev"
              type="button"
              aria-expanded={modelsOpen === role}
              onclick={() => (modelsOpen = modelsOpen === role ? null : role)}
            >
              <span class="nm">{held.current.file}</span>
              <Chevron />
            </button>
            {#if modelsOpen === role}
              <div class="list" aria-label="{roleWord(role)} model">
                {#each held.choices as choice (choice.file)}
                  <button
                    class="drow"
                    class:on={choice.current}
                    type="button"
                    aria-pressed={choice.current}
                    onclick={() => switchTo(role, choice.file)}
                  >
                    <span class="nm">{choice.file}</span>
                  </button>
                {/each}
                {#if held.choices.length === 1}
                  <div class="note">
                    nothing else on this machine takes this role &middot; the models page installs
                    more
                  </div>
                {/if}
              </div>
            {/if}
          {/if}
        </div>
      {/if}
    {/each}
  {:else}
    <!-- The read has not landed: a panel that drew nothing here would read
         as a session with no models rather than one still being read. -->
    <div class="ax">
      <div class="lbl">MODELS</div>
      <div class="note">reading the models this session runs...</div>
    </div>
  {/if}

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
      <Chevron />
    </button>
    {#if listOpen}
      <div class="list" aria-label="Input device">
        {#if walking}
          <div class="note">Looking for inputs...</div>
        {:else if refused !== null}
          <div class="note bad">{refused}</div>
        {:else}
          <button class="drow" type="button" onclick={() => choose(null)}>
            <span class="nm">{defaultRow()}</span>
          </button>
          {#each devices ?? [] as held, at (held.id)}
            <button class="drow" type="button" onclick={() => choose(held.id)}>
              <span class="nm">{held.label || `Microphone ${at + 1}`}</span>
            </button>
          {/each}
          {#if devices !== null && devices.length === 0}
            <div class="note">No input devices found.</div>
          {/if}
        {/if}
      </div>
    {/if}
    {#if (devices ?? []).some((held) => held.label === '')}
      <div class="note">
        the browser's own list &#183; names appear once this page has been allowed the microphone
      </div>
    {/if}
  </div>

  <div class="ft">
    <button class="rst" type="button" onclick={reset}>Reset all to defaults</button>
  </div>
</div>

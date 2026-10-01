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

  $effect(() => {
    return connection.onMessage((message) => {
      if (message.kind !== 'devices') return;
      devices = message.devices;
      configured = message.configured;
      walked = true;
      listOpen = true;
    });
  });

  function openList(): void {
    listOpen = true;
    if (walked) return;
    // One ask for one walk: a socket that is not open answers `false`, and the
    // row then keeps drawing what it already knows rather than an empty list.
    connection.devices();
  }

  /** The device in force: the process pick, else the pin, else the system's own. */
  const inForceDevice = $derived.by(() => {
    if (device === 'system') return 'System default';
    const picked = typeof device === 'object' && device !== null ? device.device : null;
    const wanted = picked ?? configured;
    if (wanted === null) return 'System default';
    return devices?.find((held) => held.id === wanted)?.name ?? wanted;
  });

  function choose(pick: 'system' | { device: string }): void {
    listOpen = false;
    void connection.dispatch({ set_dictate_device: { key: slot, pick } });
  }
</script>

<div class="pop" role="dialog" aria-label="Dictation">
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
    <button class="dev" type="button" aria-expanded={listOpen} onclick={openList}>
      <Icon name="mic" />
      <span class="nm">{inForceDevice}</span>
      <span class="chev">&#9656;</span>
    </button>
    {#if listOpen}
      <div class="list" aria-label="Input device">
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

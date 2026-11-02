<script lang="ts">
  /**
   * The voice control beside the mic: the switch for this client on this
   * seat, and the hold that becomes an utterance.
   *
   * A press on a switched-off control turns voice on; while it is on, a
   * press starts the hold and the release sends what was said, and a tap
   * that said nothing is the way back off.
   *
   * Mounted per seat: the page keys it by the seat, so a seat change tears
   * the old instance down and its own voice_mode off goes with it.
   */
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { boxKey } from '../composer/box.svelte';
  import { MAX_HOLD_SECONDS } from './capture.svelte';
  import { voiceFor } from './registry';
  import { connectionPort, type OpenCapture } from './voice.svelte';

  let {
    connection,
    slot,
    element = () => {},
    open,
  }: {
    /** The narrowed connection the composer holds: its commands, its store
     * and its status, which is everything the switch needs. */
    connection: Pick<Connection, 'store' | 'dispatch' | 'onStatus'>;
    slot: SessionSlot;
    /** The button, for a surface that keeps the caret on it. */
    element?: (el: HTMLElement | null) => void;
    /** How a hold opens the microphone; a seam so a test needs no audio stack. */
    open?: OpenCapture;
  } = $props();

  /** A release sooner than this, having said nothing, is a tap: it turns
   * voice off rather than sending a clip of pure silence. */
  const TAP_MS = 300;

  // The seat's key is fixed for this instance's life: the composer keys the
  // control by the seat, and the registry keeps the voice across an ask's
  // body swap, which unmounts this instance without leaving the seat.
  // svelte-ignore state_referenced_locally
  const voiceKey = boxKey(slot);
  // svelte-ignore state_referenced_locally
  const voice = voiceFor(voiceKey, connectionPort(connection, slot), open);
  let pressedAt = 0;
  let holding = false;
  let capTimer: ReturnType<typeof setTimeout> | null = null;
  let button = $state<HTMLElement | null>(null);

  // The seat's store is where every frame the server sends lands, so each
  // change is the moment to take the voice frames out of its tail. A socket
  // that goes away takes the client off the seat's listener set with it, so
  // the switch goes off rather than showing a listener nobody has.
  $effect(() => {
    const store = connection.store({ session: slot });
    const stop = store?.value.subscribe(() => voice.pump());
    const offStatus = connection.onStatus((status) => {
      if (status === 'open') voice.resume();
      else voice.reset();
    });
    return () => {
      if (capTimer !== null) clearTimeout(capTimer);
      // **A hold interrupted by an unmount stops with it.** The pointerup
      // never reaches a removed element, so without this the microphone keeps
      // recording and the next press sends the audio held through the ask.
      voice.cancelTalk();
      stop?.();
      offStatus();
      // The voice itself is the registry's, not this instance's: a body swap
      // unmounts the control without leaving the seat.
    };
  });

  $effect(() => {
    element(button);
    return () => element(null);
  });

  function press(): void {
    pressedAt = Date.now();
    if (!voice.on) {
      voice.turnOn();
      return;
    }
    holding = true;
    // A hold past the bound ends itself rather than running until release.
    capTimer = setTimeout(() => {
      if (holding) release();
    }, MAX_HOLD_SECONDS * 1000);
    void voice.startTalk();
  }

  function release(): void {
    if (capTimer !== null) {
      clearTimeout(capTimer);
      capTimer = null;
    }
    if (!holding) return;
    holding = false;
    const frames = voice.clip?.frames ?? 0;
    if (Date.now() - pressedAt < TAP_MS && frames === 0) {
      voice.cancelTalk();
      voice.turnOff();
      return;
    }
    void voice.endTalk();
  }

  function key(event: KeyboardEvent): void {
    if (event.key !== ' ' && event.key !== 'Enter') return;
    event.preventDefault();
    if (event.repeat) return;
    if (event.type === 'keydown') press();
    else release();
  }

  const label = $derived(
    voice.state === 'talking'
      ? 'talking'
      : voice.state === 'speaking'
        ? 'speaking'
        : voice.state === 'listening'
          ? 'listening'
          : 'voice off',
  );
  /** The room, when more than this client is in it. */
  const room = $derived(voice.listeners > 1 ? `${voice.listeners} clients listening` : null);
  const title = $derived(
    voice.on
      ? `hold to talk, tap to turn voice off${room === null ? '' : ` \u{b7} ${room}`}`
      : `turn voice on${room === null ? '' : ` \u{b7} ${room}`}`,
  );
</script>

<button
  class="voice"
  class:on={voice.on}
  type="button"
  bind:this={button}
  data-state={voice.state}
  data-listeners={voice.listeners}
  {title}
  aria-label={voice.on ? `${label} \u{b7} ${title}` : title}
  aria-pressed={voice.on}
  onpointerdown={(event: PointerEvent) => {
    if (event.button !== 0) return;
    event.preventDefault();
    press();
  }}
  onpointerup={release}
  onpointerleave={release}
  onpointercancel={release}
  onkeydown={key}
  onkeyup={key}
  oncontextmenu={(event: MouseEvent) => event.preventDefault()}
>
  <Icon name={voice.state === 'speaking' ? 'sound' : voice.on ? 'mic' : 'mic-off'} />
  {#if room !== null}
    <span class="room" aria-hidden="true">{voice.listeners}</span>
  {/if}
  {#if voice.state === 'talking'}
    <span class="dot" aria-hidden="true"></span>
    <span class="meter" aria-hidden="true"
      ><span class="fill" style:width={`${Math.round((voice.clip?.level ?? 0) * 100)}%`}
      ></span></span
    >
  {/if}
</button>
{#if voice.line !== null}
  <span class="vline" role="status">{voice.line}</span>
{/if}

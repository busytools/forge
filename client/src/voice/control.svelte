<script lang="ts">
  /**
   * The voice control beside the mic: the switch for this client on this
   * seat, and the hold that becomes an utterance.
   *
   * A press on a switched-off control turns voice on. While it is on a press
   * starts the hold and the release sends what was said - a tap that said
   * nothing is the way back off, so the same control is the door both ways.
   * Every state carries a shape rather than a colour alone: a struck-through
   * mic when voice is off, a plain one listening, the same with a mark while
   * it records, and a speaker while a reply plays.
   *
   * **Mounted per seat.** The page keys this control by the seat it draws, so
   * a seat change tears the old instance down - its own voice_mode off goes
   * with it - rather than leaving a switch on for a seat nobody shows.
   */
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { Voice, connectionPort } from './voice.svelte';

  let {
    connection,
    slot,
    element = () => {},
  }: {
    /** The narrowed connection the composer holds: its commands, its store
     * and its status, which is everything the switch needs. */
    connection: Pick<Connection, 'store' | 'dispatch' | 'onStatus'>;
    slot: SessionSlot;
    /** The button, for a surface that keeps the caret on it. */
    element?: (el: HTMLElement | null) => void;
  } = $props();

  /** A release sooner than this, having said nothing, is a tap: it turns
   * voice off rather than sending a clip of pure silence. */
  const TAP_MS = 300;

  // Both props are fixed for this instance's life: the composer keys the
  // control by the seat, so a new seat is a new control.
  // svelte-ignore state_referenced_locally
  const voice = new Voice(connectionPort(connection, slot));
  let pressedAt = 0;
  let holding = false;
  let button = $state<HTMLElement | null>(null);

  // The seat's store is where every frame the server sends lands, so each
  // change is the moment to take the voice frames out of its tail. A socket
  // that goes away takes the client off the seat's listener set with it, so
  // the switch goes off rather than showing a listener nobody has.
  $effect(() => {
    const store = connection.store({ session: slot });
    const stop = store?.value.subscribe(() => voice.pump());
    const offStatus = connection.onStatus((status) => {
      if (status !== 'open') voice.reset();
    });
    return () => {
      stop?.();
      offStatus();
      voice.detach();
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
    void voice.startTalk();
  }

  function release(): void {
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
</script>

<button
  class="voice"
  class:on={voice.on}
  type="button"
  bind:this={button}
  data-state={voice.state}
  title={voice.on ? 'hold to talk, tap to turn voice off' : 'turn voice on'}
  aria-label={voice.on ? `${label} \u{b7} hold to talk, tap to turn voice off` : 'turn voice on'}
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
  {#if voice.state === 'talking'}
    <span class="dot" aria-hidden="true"></span>
  {/if}
</button>
{#if voice.line !== null}
  <span class="vline" role="status">{voice.line}</span>
{/if}

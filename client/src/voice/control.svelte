<script lang="ts">
  /**
   * The agent's voice for this seat: one switch for this client, on or off.
   * Off by default; the dictation mic is how the user speaks, so this control
   * carries no microphone of its own.
   */
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { SessionSlot } from '../wire/types';
  import { boxKey } from '../composer/box.svelte';
  import { voiceFor } from './registry';
  import { connectionPort } from './voice.svelte';

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

  // The seat's key is fixed for this instance's life: the composer keys the
  // control by the seat, and the registry keeps the voice across an ask's
  // body swap, which unmounts this instance without leaving the seat.
  // svelte-ignore state_referenced_locally
  const voiceKey = boxKey(slot);
  // svelte-ignore state_referenced_locally
  const voice = voiceFor(voiceKey, connectionPort(connection, slot));
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
    // A return to a seat the reader had on: the intent outlived the seat
    // change, so the switch comes back on with the control.
    voice.resume();
    return () => {
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

  function toggle(): void {
    if (voice.on) voice.turnOff();
    else voice.turnOn();
  }

  /** The room, when more than this client is in it. */
  const room = $derived(voice.listeners > 1 ? `${voice.listeners} clients listening` : null);
  const title = $derived(
    voice.on
      ? `tap to turn off voice${room === null ? '' : ` \u{b7} ${room}`}`
      : `tap to turn on voice${room === null ? '' : ` \u{b7} ${room}`}`,
  );
  const label = $derived(
    voice.state === 'speaking' ? 'the agent is speaking' : voice.on ? 'voice on' : 'voice off',
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
  aria-label={`${label} \u{b7} ${title}`}
  aria-pressed={voice.on}
  onclick={toggle}
  oncontextmenu={(event: MouseEvent) => event.preventDefault()}
>
  <Icon name={voice.on ? 'sound' : 'sound-off'} />
  {#if room !== null}
    <span class="room" aria-hidden="true">{voice.listeners}</span>
  {/if}
  {#if voice.state === 'speaking'}
    <span class="dot speaking" aria-hidden="true"></span>
  {/if}
</button>
{#if voice.line !== null}
  <span class="vline" role="status">{voice.line}</span>
{/if}

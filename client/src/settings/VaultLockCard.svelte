<script lang="ts">
  import InstanceCard from './InstanceCard.svelte';
  import type { VaultWire } from '../wire/home';

  /**
   * The vault area's account card: whether the shared session holds its keys
   * open, the idle bound it locks on, and the manual lock.
   *
   * **It is app-level** - one vault for the whole config dir - so it names no
   * seat, and the lock press is the same command from any page. The device
   * enrolments draw beside it, because a device is a shortcut into this
   * session rather than a session of its own.
   */
  let {
    state,
    onlock,
  }: {
    state: VaultWire | null;
    onlock: () => void;
  } = $props();

  const idle = $derived(
    state === null || state.idle_minutes === null
      ? 'no idle lock'
      : `locks after ${state.idle_minutes} ${state.idle_minutes === 1 ? 'minute' : 'minutes'} without use`,
  );
</script>

<InstanceCard label="vault" count={null} headingId="vault-lock">
  {#if state === null}
    <p class="settings-empty">No [vault] is configured, so there is no session to lock.</p>
  {:else}
    <div class="settings-setting">
      <span class="settings-setting-label">
        <code>{state.locked ? 'locked' : 'unlocked'}</code>
        <span class="settings-dim">{idle}</span>
      </span>
      <span class="settings-setting-value">
        {#if state.locked}
          <span class="settings-dim">
            the next vault call asks for the master password or a device gesture
          </span>
        {:else}
          <button type="button" class="settings-btn ghost" onclick={onlock}>lock</button>
        {/if}
      </span>
    </div>
  {/if}
</InstanceCard>

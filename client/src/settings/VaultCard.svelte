<script lang="ts">
  import InstanceCard from './InstanceCard.svelte';
  import { ago } from '../components/age';
  import type { DeviceGate } from '../biometric/device';
  import { enrolmentIdIfMinted } from '../biometric/vault-enrolment';
  import { VAULT_PURPOSE } from '../biometric/vault';
  import type { BiometricEnrolment, VaultWire } from '../wire/home';

  /**
   * The vault: the shared session's state and the clients enrolled against
   * it, in one card.
   *
   * **The action follows both facts**: an unenrolled client (or one whose
   * server row is gone) enrols, an enrolled client unlocks while the vault is
   * locked, and an enrolled client with the vault open has no action - the
   * session line's `lock` door is the session's own control. The press never
   * dead-ends on "unlock the vault first": the page opens the unlock dialog
   * in place.
   *
   * **A revoke drops that purpose's blob only** - the client stays, the
   * account is untouched, and the client's next start is the master password
   * until it enrols again. The list is the core's runtime state, not a
   * `forge.toml` key, and an unreadable read says so rather than drawing as
   * none.
   */
  let {
    vault,
    read,
    device = { outcome: 'reading' },
    onaction = () => {},
    onrevoke = () => {},
    onlock = () => {},
  }: {
    /** `undefined` is "the home has not been read yet"; `null` is a config
     *  with no `[vault]` at all. */
    vault: VaultWire | null | undefined;
    read: { outcome: 'read'; value: BiometricEnrolment[] } | { outcome: 'failed'; value: string };
    device?: DeviceGate;
    onaction?: (action: 'enrol' | 'unlock') => void;
    onrevoke?: (enrolment: string, purpose: string) => void;
    onlock?: () => void;
  } = $props();

  const nowSeconds = Math.floor(Date.now() / 1000);
  const failure = $derived(read.outcome === 'failed' ? read.value : null);
  const clients = $derived(read.outcome === 'read' ? read.value : []);
  const count = $derived(read.outcome === 'read' ? clients.length : null);
  const idle = $derived(
    vault === null || vault === undefined || vault.idle_minutes === null
      ? 'no idle lock'
      : `locks after ${vault.idle_minutes} ${vault.idle_minutes === 1 ? 'minute' : 'minutes'} without use`,
  );

  /** This client's row in the core's list, when the id was ever minted. */
  const mine = $derived(clients.find((held) => held.id === enrolmentIdIfMinted()) ?? null);
  const serverHasVault = $derived(
    mine?.purposes.some((purpose) => purpose.purpose === VAULT_PURPOSE) ?? false,
  );
  const noServerRow = $derived(
    device.outcome === 'enrolled' && read.outcome === 'read' && !serverHasVault,
  );

  /** This client's own state, in one line. */
  const deviceLine = $derived.by(() => {
    switch (device.outcome) {
      case 'reading':
        return 'checking this client';
      case 'enrolled':
        if (noServerRow) return 'enrolled here, but the server has no row - enrol again';
        return 'enrolled - the gesture unlocks the vault here';
      case 'enrolling':
        return 'enrolling this client';
      case 'ready':
        return 'not enrolled';
      case 'unavailable':
        return `cannot use this device's gesture: ${device.reason}`;
      case 'failed':
        return `the enrolment did not land: ${device.reason}`;
    }
  });

  /** The one action this client's state offers, if any. */
  const action = $derived.by((): { kind: 'enrol' | 'unlock'; label: string } | null => {
    switch (device.outcome) {
      case 'ready':
        return { kind: 'enrol', label: 'enrol this client' };
      case 'failed':
        return { kind: 'enrol', label: 'enrol again' };
      case 'enrolled':
        if (noServerRow) return { kind: 'enrol', label: 'enrol again' };
        return vault?.locked === true ? { kind: 'unlock', label: 'unlock' } : null;
      case 'unavailable':
        // **The gate cannot answer, but the password still can.** The
        // contract keeps it reachable whenever no biometric is enrolled, and
        // this card is the client's only unlock door.
        return vault?.locked === true ? { kind: 'unlock', label: 'unlock' } : null;
      default:
        return null;
    }
  });

  /** A release's stamp, or the words for one that never happened. */
  function lastUsed(at: { secs_since_epoch: number } | null | undefined): string {
    if (at === null || at === undefined) return 'never used';
    return `last used ${ago(at.secs_since_epoch, nowSeconds)} ago`;
  }
</script>

<InstanceCard label="vault" headingId="vault-lock">
  {#if vault === undefined}
    <p class="settings-empty">The vault state has not been read yet.</p>
  {:else if vault === null}
    <p class="settings-empty">No [vault] is configured, so there is no session to enrol against.</p>
  {:else}
    <div class="settings-setting">
      <span class="settings-setting-label">
        <code>{vault.locked ? 'locked' : 'unlocked'}</code>
        <span class="settings-dim">{idle}</span>
      </span>
      <span class="settings-setting-value">
        {#if vault.locked}
          <span class="settings-dim">
            the next vault call asks for the master password or a device gesture
          </span>
        {:else}
          <button type="button" class="settings-btn ghost" onclick={onlock}>lock</button>
        {/if}
      </span>
    </div>

    <div class="settings-card-head">
      <span class="settings-card-name">clients</span>
      {#if count !== null}<span class="settings-card-count">{count}</span>{/if}
      <span class="settings-card-grow"></span>
    </div>

    <div class="settings-setting" data-device="this">
      <span class="settings-setting-label">
        <code>this client</code>
        <span class="settings-dim">{deviceLine}</span>
      </span>
      <span class="settings-setting-value">
        {#if action !== null}
          <button type="button" class="settings-btn ghost" onclick={() => onaction(action.kind)}>
            {action.label}
          </button>
        {/if}
      </span>
    </div>

    {#if failure !== null}
      <p class="settings-empty">The enrolment list could not be read: {failure}</p>
    {:else if clients.length === 0}
      <p class="settings-empty">
        No client has enrolled. Enrol this client above while the vault is unlocked, and later
        starts can use its authenticator instead.
      </p>
    {:else}
      {#each clients as enrolment (enrolment.id)}
        <div class="settings-setting" data-device={enrolment.device}>
          <span class="settings-setting-label">
            <code>{enrolment.device}</code>
            {#if enrolment.id === enrolmentIdIfMinted()}
              <span class="settings-dim">this client</span>
            {/if}
            <span class="settings-dim"
              >enrolled {ago(enrolment.created_at.secs_since_epoch, nowSeconds)} ago</span
            >
          </span>
          <span class="settings-setting-value">
            {#each enrolment.purposes as purpose (purpose.purpose)}
              <span class="biometric-purpose">
                <code>{purpose.purpose}</code>
                <span class="settings-dim">{lastUsed(purpose.last_used_at)}</span>
                <button
                  type="button"
                  class="settings-btn ghost"
                  aria-label={`revoke ${purpose.purpose} on ${enrolment.device}`}
                  onclick={() => onrevoke(enrolment.id, purpose.purpose)}>revoke</button
                >
              </span>
            {/each}
          </span>
        </div>
      {/each}
    {/if}
  {/if}
</InstanceCard>

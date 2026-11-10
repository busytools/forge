<script lang="ts">
  import InstanceCard from './InstanceCard.svelte';
  import { ago } from '../components/age';
  import type { DeviceGate } from '../biometric/device';
  import { enrolmentIdIfMinted } from '../biometric/vault-enrolment';
  import type { BiometricEnrolment } from '../wire/home';

  /**
   * The vault area's device enrolments: every device the core holds, each
   * purpose it enrolled and when it was last released, with a revoke per
   * purpose.
   *
   * **A revoke drops that purpose's blob only** - the device stays, the
   * account is untouched, and the device's next start is the master password
   * until it enrols again. **Enrolling is this device's own action**, drawn
   * here: nothing enrols automatically, so a revoked device that is still
   * connected cannot re-register itself. The list is read from the home; the
   * frame that announces a change is what redraws this card.
   */
  let {
    read,
    onrevoke,
    device = { outcome: 'reading' },
    onenrol = () => {},
  }: {
    read: { outcome: 'read'; value: BiometricEnrolment[] } | { outcome: 'failed'; value: string };
    onrevoke: (enrolment: string, purpose: string) => void;
    device?: DeviceGate;
    onenrol?: () => void;
  } = $props();

  const nowSeconds = Math.floor(Date.now() / 1000);
  // Narrowed once, so the template's branches carry the types they draw:
  // `{:else if}` does not narrow a prop for the type-aware lint.
  const failure = $derived(read.outcome === 'failed' ? read.value : null);
  const devices = $derived(read.outcome === 'read' ? read.value : []);
  // **A failed read is not zero devices.** The head band draws no count when
  // the list could not be read, rather than a `0` that reads as the truth.
  const count = $derived(read.outcome === 'read' ? devices.length : null);

  /** A release's stamp, or the words for one that never happened. */
  function lastUsed(at: { secs_since_epoch: number } | null | undefined): string {
    if (at === null || at === undefined) return 'never used';
    return `last used ${ago(at.secs_since_epoch, nowSeconds)} ago`;
  }

  /** This device's row in the core's list, when the id was ever minted. */
  const mine = $derived(devices.find((held) => held.id === enrolmentIdIfMinted()) ?? null);
  /** Whether the core still holds this device's vault purpose. */
  const serverHasVault = $derived(
    mine?.purposes.some((purpose) => purpose.purpose === 'vault') ?? false,
  );

  /** This device's own state, in one line. */
  const deviceLine = $derived.by(() => {
    switch (device.outcome) {
      case 'reading':
        return 'checking this device';
      case 'enrolled':
        // **Enrolled here is not enrolled on the server**, but only a list
        // that was read can say so: a failed read is not "no row".
        if (read.outcome !== 'read') return 'enrolled - the gesture unlocks the vault here';
        return serverHasVault
          ? 'enrolled - the gesture unlocks the vault here'
          : 'enrolled here, but the server has no row - enrol again';
      case 'enrolling':
        return 'enrolling this device';
      case 'ready':
        return 'not enrolled - enrol while the vault is unlocked';
      case 'needs_unlock':
        return 'not enrolled - unlock the vault first';
      case 'unavailable':
        return `cannot unlock here: ${device.reason}`;
      case 'failed':
        return `the enrolment did not land: ${device.reason}`;
    }
  });
  // The press is offered unless the gate cannot hold a key at all, or a press
  // is already running. **It is a replacing re-enrol**, so it is the route
  // back from a lost server row and from a stale blob alike - the local item
  // cannot tell the two apart, and hiding the press for one hid it for both.
  const canEnrol = $derived(
    device.outcome !== 'reading' &&
      device.outcome !== 'unavailable' &&
      device.outcome !== 'enrolling',
  );
</script>

<InstanceCard label="devices" {count} countNoun={null} headingId="biometric-devices">
  <div class="settings-setting" data-device="this">
    <span class="settings-setting-label">
      <code>this device</code>
      <span class="settings-dim">{deviceLine}</span>
    </span>
    <span class="settings-setting-value">
      {#if canEnrol}
        <button type="button" class="settings-btn ghost" onclick={onenrol}>enrol</button>
      {/if}
    </span>
  </div>
  {#if failure !== null}
    <p class="settings-empty">
      The enrolment list could not be read: {failure}
    </p>
  {:else if devices.length === 0}
    <p class="settings-empty">
      No device has enrolled. Enrol this device above while the vault is unlocked, and later starts
      can use its authenticator instead.
    </p>
  {:else}
    {#each devices as enrolment (enrolment.id)}
      <div class="settings-setting" data-device={enrolment.device}>
        <span class="settings-setting-label">
          <code>{enrolment.device}</code>
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
</InstanceCard>

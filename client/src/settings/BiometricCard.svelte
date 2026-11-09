<script lang="ts">
  import InstanceCard from './InstanceCard.svelte';
  import { ago } from '../components/age';
  import type { BiometricEnrolment } from '../wire/home';

  /**
   * The vault area's device enrolments: every device the core holds, each
   * purpose it enrolled and when it was last released, with a revoke per
   * purpose.
   *
   * **A revoke drops that purpose's blob only** - the device stays, the
   * account is untouched, and the device's next start is the master password
   * until it enrols again. The list is read from the home; the frame that
   * announces a change is what redraws this card.
   */
  let {
    read,
    onrevoke,
  }: {
    read: { outcome: 'read'; value: BiometricEnrolment[] } | { outcome: 'failed'; value: string };
    onrevoke: (enrolment: string, purpose: string) => void;
  } = $props();

  const nowSeconds = Math.floor(Date.now() / 1000);
  // Narrowed once, so the template's branches carry the types they draw:
  // `{:else if}` does not narrow a prop for the type-aware lint.
  const failure = $derived(read.outcome === 'failed' ? read.value : null);
  const devices = $derived(read.outcome === 'read' ? read.value : []);
  const count = $derived(devices.length);

  /** A release's stamp, or the words for one that never happened. */
  function lastUsed(at: { secs_since_epoch: number } | null | undefined): string {
    if (at === null || at === undefined) return 'never used';
    return `last used ${ago(at.secs_since_epoch, nowSeconds)} ago`;
  }
</script>

<InstanceCard label="devices" {count} countNoun={null} headingId="biometric-devices">
  {#if failure !== null}
    <p class="settings-empty">
      The enrolment list could not be read: {failure}
    </p>
  {:else if devices.length === 0}
    <p class="settings-empty">
      No device has enrolled. Unlock the vault with the master password to enrol this device, and
      later starts can use its authenticator instead.
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
                onclick={() => onrevoke(enrolment.id, purpose.purpose)}>revoke</button
              >
            </span>
          {/each}
        </span>
      </div>
    {/each}
  {/if}
</InstanceCard>

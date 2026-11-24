<script lang="ts">
  import { onMount } from 'svelte';

  /**
   * The settings page's vault unlock, as a modal. Two variants: `unlock`
   * leads with the platform gesture and reveals the master password beneath
   * it, while `enrol` opens on the password because nothing is enrolled yet
   * for the gesture to release.
   *
   * **It owns no history**: the page that opens it takes the entry Android's
   * Back closes it with. It keeps Tab inside the dialog, closes on Escape,
   * and returns the keyboard to the control that opened it.
   */
  let {
    variant,
    busy = false,
    reason = null,
    ongesture = () => {},
    onpassword = () => {},
    oncancel,
    fallback = null,
  }: {
    variant: 'unlock' | 'enrol';
    busy?: boolean;
    reason?: string | null;
    ongesture?: () => void;
    onpassword?: (password: string, code: string) => void;
    oncancel: () => void;
    /** The id to focus on close when the opener no longer exists. */
    fallback?: string | null;
  } = $props();

  let dialog: HTMLElement | undefined = $state();
  let closed = false;
  let returnTo: HTMLElement | null = null;
  /** The password fields: always on for enrol, revealed by the link for unlock. */
  let revealedByLink = $state(false);
  const revealed = $derived(variant === 'enrol' || revealedByLink);
  let password = $state('');
  let code = $state('');

  const title = $derived(variant === 'enrol' ? 'unlock to enrol this client' : 'unlock the vault');
  const body = $derived(
    variant === 'enrol'
      ? 'Enrolling binds a key to this device, and the vault must be open to read its material. Enter the Vaultwarden master password; add the 2FA code when your account asks for one.'
      : 'The vault is locked. Release it with this device, or enter the Vaultwarden password.',
  );

  /** Close once and report; the page settles the history entry behind it. */
  function close(): void {
    if (closed) return;
    closed = true;
    oncancel();
  }

  onMount(() => {
    returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.querySelector<HTMLElement>('button')?.focus();

    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        event.preventDefault();
        close();
        return;
      }
      if (event.key !== 'Tab' || dialog === undefined) return;
      const focusable = [
        ...dialog.querySelectorAll<HTMLElement>('button, [href], input, select, textarea'),
      ].filter((element) => !element.hasAttribute('disabled'));
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (first === undefined || last === undefined) return;
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    addEventListener('keydown', onKey);
    return () => {
      removeEventListener('keydown', onKey);
      const target =
        returnTo?.isConnected === true
          ? returnTo
          : fallback === null
            ? null
            : document.getElementById(fallback);
      target?.focus();
    };
  });
</script>

<div class="settings-scrim">
  <div
    class="settings-dialog"
    role="dialog"
    aria-modal="true"
    aria-labelledby="vault-unlock-title"
    bind:this={dialog}
  >
    <h2 class="t" id="vault-unlock-title">{title}</h2>
    <p class="s">{body}</p>
    {#if variant === 'unlock'}
      <button type="button" class="settings-btn go" disabled={busy} onclick={ongesture}>
        {busy ? 'waiting for the device...' : 'Unlock with this device'}
      </button>
      {#if !revealed}
        <div class="reveal">
          <button type="button" class="settings-link" onclick={() => (revealedByLink = true)}>
            use the Vaultwarden password
          </button>
        </div>
      {/if}
    {/if}
    {#if revealed}
      <div class="fields">
        <label>
          master password
          <input type="password" bind:value={password} placeholder="master password" />
        </label>
        <label>
          2FA code (optional)
          <input
            type="text"
            inputmode="numeric"
            autocomplete="one-time-code"
            bind:value={code}
            placeholder="2FA code (optional)"
          />
        </label>
      </div>
    {/if}
    {#if reason !== null}
      <p class="why" role="alert">{reason}</p>
    {/if}
    <div class="acts">
      <button type="button" class="settings-btn ghost" onclick={close}>cancel</button>
      {#if revealed}
        <button
          type="button"
          class="settings-btn go"
          disabled={busy}
          onclick={() => onpassword(password, code)}
        >
          {busy ? 'unlocking...' : variant === 'enrol' ? 'unlock and enrol' : 'unlock'}
        </button>
      {/if}
    </div>
  </div>
</div>

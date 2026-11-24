<script lang="ts">
  import { onMount } from 'svelte';

  import Field from '../composer/Field.svelte';

  /**
   * The settings page's vault unlock, as a modal. Two variants: `unlock`
   * leads with the platform gesture and reveals the master password beneath
   * it, while `enrol` opens on the password because nothing is enrolled yet
   * for the gesture to release.
   *
   * **It owns no history**: the page that opens it takes the entry Android's
   * Back closes it with. It keeps Tab inside the dialog, closes on Escape,
   * and returns the keyboard to the control that opened it. **The enrol
   * variant starts in the password box**; the unlock variant starts on the
   * gesture, which is its first button.
   */
  let {
    variant,
    busy = false,
    reason = null,
    biometric = true,
    ongesture = () => {},
    onpassword = () => {},
    oncancel,
    fallback = null,
  }: {
    variant: 'unlock' | 'enrol';
    busy?: boolean;
    reason?: string | null;
    /** Whether the gate's gesture is a biometric one; false is the device
        password fallback, and the button names it. */
    biometric?: boolean;
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
  let passwordEl: HTMLInputElement | null = null;

  const title = $derived(variant === 'enrol' ? 'unlock to enrol this client' : 'unlock the vault');
  const body = $derived(
    variant === 'enrol'
      ? 'Enrolling binds a key to this device, and the vault must be open to read its material. Enter the Vaultwarden master password; add the 2FA code when your account asks for one.'
      : 'The vault is locked. Release it with this device, or enter the Vaultwarden password.',
  );
  const gesture = $derived(biometric ? 'this device' : 'the device password');

  /** Submit the password path on Enter, the way the dock's unlock card does. */
  function onFieldKey(event: KeyboardEvent): void {
    if (event.key !== 'Enter' || busy) return;
    event.preventDefault();
    onpassword(password, code);
  }

  /** Close once and report; the page settles the history entry behind it. */
  function close(): void {
    if (closed) return;
    closed = true;
    oncancel();
  }

  onMount(() => {
    returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    if (variant === 'unlock') {
      dialog?.querySelector<HTMLElement>('button')?.focus();
    } else {
      // **The enrol variant starts in the password box**, not on cancel: the
      // page asked for a password. Queued after the mount so the capture
      // above still reads the opener, and the keyboard returns to it on
      // close.
      queueMicrotask(() => passwordEl?.focus());
    }

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
        {busy ? `waiting for ${gesture}...` : `Unlock with ${gesture}`}
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
        <label for="vault-unlock-password">master password</label>
        <Field
          editor="settings"
          element="input"
          type="password"
          id="vault-unlock-password"
          bind:value={password}
          placeholder="master password"
          aria={{ label: 'the vault master password' }}
          onkeydown={onFieldKey}
          field={(element: HTMLElement | null) => {
            passwordEl = element as HTMLInputElement | null;
          }}
        />
        <label for="vault-unlock-code">2FA code (optional)</label>
        <Field
          editor="settings"
          element="input"
          type="text"
          id="vault-unlock-code"
          bind:value={code}
          placeholder="2FA code (optional)"
          autocomplete="one-time-code"
          inputmode="numeric"
          aria={{ label: 'the two-factor code' }}
          onkeydown={onFieldKey}
        />
      </div>
    {/if}
    {#if reason !== null}
      <p class="why" role="alert">{reason}</p>
    {/if}
    <div class="acts vault-acts">
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

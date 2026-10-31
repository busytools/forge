<script lang="ts">
  import { onMount } from 'svelte';

  /**
   * One destructive confirm, as a modal. **It owns no history**: the page
   * that opens it takes the entry Android's Back closes it with, so one
   * listener settles every pop. It keeps Tab inside the dialog, closes on
   * Escape, and returns the keyboard to the control that opened it - or to
   * `fallback`'s element when that control is gone.
   */
  let {
    title,
    body,
    confirm,
    cancel = 'cancel',
    fallback = null,
    onconfirm,
    oncancel,
  }: {
    title: string;
    body: string;
    confirm: string;
    cancel?: string;
    /** The id to focus on close when the opener no longer exists. */
    fallback?: string | null;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  let dialog: HTMLElement | undefined = $state();
  let closed = false;
  let returnTo: HTMLElement | null = null;

  /** Close once and report; the page settles the history entry behind it. */
  function close(confirmed: boolean): void {
    if (closed) return;
    closed = true;
    if (confirmed) onconfirm();
    else oncancel();
  }

  onMount(() => {
    returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.querySelector<HTMLElement>('button')?.focus();

    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        event.preventDefault();
        close(false);
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
    aria-labelledby="settings-confirm-title"
    bind:this={dialog}
  >
    <h2 class="t" id="settings-confirm-title">{title}</h2>
    <p class="s">{body}</p>
    <div class="acts">
      <button type="button" class="settings-btn ghost" onclick={() => close(false)}>{cancel}</button
      >
      <button type="button" class="settings-btn danger" onclick={() => close(true)}>
        {confirm}
      </button>
    </div>
  </div>
</div>

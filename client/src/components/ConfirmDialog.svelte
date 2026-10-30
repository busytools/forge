<script lang="ts">
  import { onMount } from 'svelte';

  /**
   * One destructive confirm, as a modal. It takes a history entry of its own,
   * so Android's Back closes what it opened; Tab stays inside the dialog,
   * Escape closes it, and the keyboard returns to the control that opened
   * it. The caller owns what the two buttons mean.
   */
  let {
    title,
    body,
    confirm,
    cancel = 'cancel',
    onconfirm,
    oncancel,
  }: {
    title: string;
    body: string;
    confirm: string;
    cancel?: string;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  const ENTRY = 'forgeSettingsConfirm';
  let dialog: HTMLElement | undefined = $state();
  let closed = false;
  let returnTo: HTMLElement | null = null;

  function stateOf(): Record<string, unknown> {
    const held: unknown = history.state;
    return typeof held === 'object' && held !== null ? (held as Record<string, unknown>) : {};
  }

  /** Close once and report; the history entry is cleaned up behind it. */
  function close(confirmed: boolean, fromPop = false): void {
    if (closed) return;
    closed = true;
    if (!fromPop && stateOf()[ENTRY] === true) history.back();
    if (confirmed) onconfirm();
    else oncancel();
    returnTo?.focus();
  }

  onMount(() => {
    returnTo = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    history.pushState({ ...stateOf(), [ENTRY]: true }, '', location.href);
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
    const onPop = (): void => {
      close(false, true);
    };

    addEventListener('keydown', onKey);
    addEventListener('popstate', onPop);
    return () => {
      removeEventListener('keydown', onKey);
      removeEventListener('popstate', onPop);
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

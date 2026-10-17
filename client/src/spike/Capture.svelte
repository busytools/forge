<script lang="ts">
  /**
   * The embed-capture spike's scratch door: present the system content
   * picker from the shell's own GUI context, then draw the picked window
   * over this page. Never merges.
   */
  import { onMount } from 'svelte';

  let said = $state<string | null>(null);
  /**
   * The command in flight, by name. **A pending call must not freeze the
   * other buttons**: the first pick attempt hung forever behind a picker
   * that never presented, and the disabled buttons read as "cannot press".
   */
  let pending = $state<string | null>(null);
  let picking = $state(false);
  let inShell = $state(false);
  /** The strip the overlay must leave clear, so the door stays clickable. */
  const STRIP = 72;

  onMount(() => {
    // Only the shell answers these; a plain browser tab draws nothing.
    inShell = '__TAURI_INTERNALS__' in window;
  });

  async function call(command: string, args: Record<string, number> = {}): Promise<void> {
    pending = command;
    const watchdog = window.setTimeout(() => {
      // Still pending: say so and hand the buttons back, so a call that
      // hangs (no observer answer) does not brick the door.
      pending = null;
      said = `${command} is still pending after 60 s - if no system dialog appeared, it never presented`;
    }, 60_000);
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const answer: unknown = await invoke(command, args);
      said = answer === null || answer === undefined ? 'ok' : JSON.stringify(answer);
      if (command === 'capture_pick') picking = true;
      if (command === 'capture_hide') picking = false;
    } catch (why) {
      said = String(why);
    } finally {
      window.clearTimeout(watchdog);
      pending = null;
    }
  }

  function pick(): void {
    said = 'picker asked for - a system dialog should appear';
    void call('capture_pick');
  }

  function show(): void {
    void call('capture_show', {
      x: 0,
      y: STRIP,
      width: window.innerWidth,
      height: window.innerHeight - STRIP,
    });
  }

  function hide(): void {
    void call('capture_hide');
  }
</script>

{#if inShell}
  <div class="spike">
    <button type="button" onclick={pick} disabled={pending === 'capture_pick'}>
      1. present the capture picker
    </button>
    <button type="button" onclick={show} disabled={pending === 'capture_show' || !picking}>
      2. show the capture
    </button>
    <button type="button" onclick={hide} disabled={pending === 'capture_hide'}>3. hide</button>
    <button
      type="button"
      onclick={() => void call('capture_grant')}
      disabled={pending === 'capture_grant'}
    >
      1b. grant screen recording (fallback)
    </button>
    {#if said !== null}
      <span class="said" role="status">{said}</span>
    {/if}
  </div>
{/if}

<style>
  .spike {
    align-items: center;
    background: var(--s2);
    color: var(--text);
    display: flex;
    font-size: var(--fs-label);
    gap: 10px;
    height: 56px;
    left: 0;
    padding: 0 10px;
    position: fixed;
    right: 0;
    top: 0;
    z-index: 40;
  }
</style>

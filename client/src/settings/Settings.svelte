<script lang="ts">
  import type { HomeRead } from '../home/live';
  import SettingsBody from './SettingsBody.svelte';
  import { settingsFrame } from './frame';

  /**
   * The settings route: it draws the document the server pushed, once one
   * has landed. There is no read verb - the frame is the read - so the only
   * states before one arrives are waiting and a frame this client could not
   * read.
   */
  let { home, mark }: { home: HomeRead; mark: string | null } = $props();

  const frame = $derived($settingsFrame);
  const cliVersion = $derived(home.wire?.cli_version?.installed ?? null);
  let selected = $state<string | null>(null);
</script>

<main class="wrap settings-page">
  {#if frame.status === 'waiting'}
    <p class="settings-pending">Reading the effective config...</p>
  {:else if frame.status === 'failed'}
    <p class="settings-pending">{frame.detail}</p>
  {:else}
    <SettingsBody
      document={frame.document}
      {mark}
      {cliVersion}
      {selected}
      onSelect={(id: string) => {
        selected = id;
      }}
    />
  {/if}
</main>

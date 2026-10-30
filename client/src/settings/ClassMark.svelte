<script lang="ts">
  import { classWords, type SettingsClass } from '../wire/settings';

  /**
   * One effect class, as a shape and its word. `icon` is the rail's compact
   * form: the shape stays and the word is there for a screen reader, so the
   * sub-nav never carries a state by colour alone. `label` is the server's
   * own word for a class this client does not know.
   */
  let {
    held,
    label = '',
    icon = false,
  }: { held: SettingsClass | null; label?: string; icon?: boolean } = $props();

  const word = $derived(held === null ? '' : classWords(held, label));
</script>

{#if held !== null}
  <span class="settings-class" class:icon data-class={held}>
    <span class="settings-class-mark" aria-hidden="true"></span>
    {#if icon}
      <span class="vh">{word}</span>
    {:else}
      <span class="settings-class-word">{word}</span>
    {/if}
  </span>
{/if}

<script lang="ts">
  import type { SettingsValue } from '../wire/settings';

  /**
   * One key's value, drawn by the shape the wire named for it. `builtIn` is
   * the effective default a `client` axis falls back to when the file does
   * not set it: display only, never the value the axes path reads.
   */
  let { value, builtIn = null }: { value: SettingsValue; builtIn?: string | null } = $props();
</script>

{#if value.kind === 'unset'}
  <span class="settings-value settings-dim">
    not set{builtIn === null ? '' : ` · built-in ${builtIn}`}
  </span>
{:else if value.kind === 'string' || value.kind === 'number' || value.kind === 'bool'}
  <span class="settings-value">{value.text}</span>
{:else if value.kind === 'list' || value.kind === 'set'}
  {#if value.items.length === 0}
    <span class="settings-value settings-dim">none</span>
  {:else}
    <span class="settings-chips">
      {#each value.items as item, index (index)}<code>{item}</code>{/each}
    </span>
  {/if}
{:else if value.kind === 'table'}
  {#if value.rows.length === 0}
    <span class="settings-value settings-dim">none</span>
  {:else}
    <span class="settings-table">
      {#each value.rows as row (row.key)}<span><code>{row.key}</code> = {row.value}</span>{/each}
    </span>
  {/if}
{:else}
  <!-- A shape this client does not know is drawn as itself rather than
       dropped: a value that arrived and drew nothing reads as one that never
       arrived. -->
  <span class="settings-value settings-dim">{value.text}</span>
{/if}

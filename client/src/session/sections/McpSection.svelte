<script lang="ts">
  import type { McpView } from '../view';
  import Section from './Section.svelte';

  /**
   * The MCP servers this session's bridge reported, with the reason a read
   * that failed came back empty rather than reading as nothing configured.
   */
  let { view }: { view: McpView } = $props();
</script>

<Section name="mcp servers" icon="mcp" summary={view.summary}>
  {#each view.rows as row (row.k)}
    <div class="kv"><span class="k">{row.k}</span><span class="v">{row.v}</span></div>
  {/each}
  {#if view.error !== null}
    <div class="note">{view.error}</div>
  {/if}
</Section>

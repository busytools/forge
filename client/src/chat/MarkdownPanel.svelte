<script lang="ts">
  import Prose from './Prose.svelte';

  /**
   * Rendered markdown in the call body's panel: the same border, ground and
   * format strip a code panel draws, with the prose inside it.
   *
   * **One component for both ways a markdown body arrives** - a Read's text
   * and a rendered diff's HTML - so the two cannot drift into two drawings of
   * the same panel.
   */
  let { html = null, text = null }: { html?: string | null; text?: string | null } = $props();
</script>

<div class="code panel markdown-panel">
  <div class="lang">markdown</div>
  <div class="code-diff">
    {#if html !== null}
      <!-- The renderer's escaped output: raw HTML is off in `renderProse`. -->
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      <div class="prose">{@html html}</div>
    {:else}
      <Prose text={text ?? ''} />
    {/if}
  </div>
</div>

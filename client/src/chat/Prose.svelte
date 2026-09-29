<script lang="ts">
  import { renderProse } from './prose';

  /**
   * A block of prose, as markdown.
   *
   * The renderer is a maintained module and its own doc says what it refuses:
   * raw HTML never reaches the page, so a session that quotes a log, a page or
   * a file from someone else's repository cannot run what it quotes.
   */
  let { text }: { text: string } = $props();

  const html = $derived(renderProse(text));
</script>

<!--
  The rendered markdown, which is data the module produced from escaped input.
  `renderProse` passes no raw HTML through, and markdown-it's own `validateLink`
  refuses a scheme that is not a link.
-->
<!-- eslint-disable-next-line svelte/no-at-html-tags -->
<div class="prose">{@html html}</div>

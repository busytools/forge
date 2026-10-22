<script lang="ts">
  import { markdownDiffHtml, patchLines } from '../format/markdown-diff';
  import type { CallBody } from './leaves';
  import { renderProse } from './prose';

  /**
   * A markdown edit, as a rendered diff: the hunk draws as markdown with the
   * removed words kept inline, struck and dimmed, and the added words drawn
   * normal. The source diff is not drawn under it - the row's `+N -M` line
   * is what says the size.
   */
  type Piece = Extract<CallBody, { kind: 'diff' } | { kind: 'hunk' }>;

  let { pieces }: { pieces: Piece[] } = $props();

  const html = $derived(markdownDiffHtml(patchLines(pieces), renderProse));
</script>

<!--
  The rendered markdown in the same panel a Read draws - border, panel ground
  and a format strip - produced by the chat's one renderer from escaped input
  (raw HTML off), with only the diff's own strike and add spans substituted
  in after it.
-->
<div class="code">
  <div class="lang">markdown</div>
  <div class="code-diff">
    <div class="prose">
      <!-- eslint-disable-next-line svelte/no-at-html-tags -->
      {@html html}
    </div>
  </div>
</div>

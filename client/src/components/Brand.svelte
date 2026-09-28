<script lang="ts">
  import { brandPath } from '../brand';
  import type { MarkName } from '../wire/types';

  let { name = null, class: klass = 'mark' }: { name?: MarkName | null; class?: string } = $props();
</script>

<!--
  Decorative wherever it is drawn: the wordmark beside it carries the name,
  so assistive tech is told to skip it.
-->
<span class={klass}>
  <svg viewBox="0 0 24 24" aria-hidden="true">{@html brandPath(name)}</svg>
</span>

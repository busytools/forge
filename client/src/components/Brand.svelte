<script lang="ts">
  import { brandPath } from '../brand';

  // A string rather than the shipped-name union: the value arrives from the
  // server, which refuses a name outside the set at boot, so what reaches
  // here is not this side's to narrow.
  let { name = null, class: klass = 'mark' }: { name?: string | null; class?: string } = $props();
</script>

<!--
  Decorative wherever it is drawn: the wordmark beside it carries the name,
  so assistive tech is told to skip it.
-->
<span class={klass}>
  <svg viewBox="0 0 24 24" aria-hidden="true">{@html brandPath(name)}</svg>
</span>

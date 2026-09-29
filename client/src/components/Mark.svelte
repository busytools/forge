<script lang="ts">
  import { markOf, type RowState } from '../home/view';

  /**
   * The lifecycle mark a row carries: one dot shape per meaning, so colour
   * is never the only signal. The row holds the colour; this holds the
   * shape.
   */
  let { state }: { state: RowState } = $props();
</script>

<span class="dot {markOf(state).dot}"></span>

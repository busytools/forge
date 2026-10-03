<script lang="ts">
  import Icon from '../components/Icon.svelte';

  /**
   * The close chip every rail row carries.
   *
   * **A sprite in a real button rather than a glyph in a span.** The terminal
   * drew `&#10005;`, sized to a monospace line box, because a character cell
   * was all it had; here the close mark is the same sprite the rest of the
   * page draws, and the thing that carries it is a control with an accessible
   * name rather than a `title` on a `span`.
   *
   * The ROW supplies the act. Which command a close sends - a worker closes
   * through `close_worker`, a project's row through `close_session` - and
   * where the reader lands afterwards are rail decisions (`session/close.ts`),
   * made where the row is drawn and not here.
   */
  let { name, onclose }: { name: string; onclose: () => void } = $props();
</script>

<button class="x" type="button" title="close {name}" aria-label="close {name}" onclick={onclose}>
  <Icon name="x" />
</button>

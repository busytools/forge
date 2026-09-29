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
   * It is disabled rather than wired to nothing: nothing in this view can
   * close a session yet, so a button that takes a click and does nothing would
   * promise work it cannot do.
   */
</script>

<button
  class="x"
  type="button"
  disabled
  title="closing a session is not available yet"
  aria-label="closing a session is not available yet"
>
  <Icon name="x" />
</button>

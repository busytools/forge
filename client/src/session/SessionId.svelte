<script lang="ts">
  import CopyButton from './CopyButton.svelte';

  /**
   * The seat's session id, with the control that copies it.
   *
   * **Eight characters on the row, the whole id on the clipboard**, which is
   * the split the view this client replaced kept: the short form is what a
   * reader compares against a transcript or another row, and the long one is
   * what a person pastes into a resume. The whole id stays reachable without
   * the click, on the `title`. Carries no label of its own: the row it sits
   * in names it.
   */
  let { id }: { id: string } = $props();

  /** How many characters of an id a row draws, as the view this client replaced drew them. */
  const SHORT = 8;
</script>

<span class="sid">
  <span class="id" title={id}>{id.slice(0, SHORT)}</span>
  <CopyButton {id} />
</span>

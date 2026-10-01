<script lang="ts">
  import { copyLabel, copyReason, type CopyOutcome } from './view';

  /**
   * The seat's session id, with the control that copies it.
   *
   * **Eight characters on the row, the whole id on the clipboard**, which is
   * the terminal's own split: the short form is what a reader compares against
   * a transcript or another row, and the long one is what a person pastes into
   * a resume. The whole id stays reachable without the click, on the `title`.
   *
   * The copy is the platform's own write, and both ways it can fail say so on
   * the control rather than passing as a click that worked: a page over a
   * plain-http origin has no `navigator.clipboard` at all, and a live one can
   * still refuse the write. Which of the two it was is the difference between
   * changing the page's origin and changing a permission, so the control names
   * it.
   */
  let { id }: { id: string } = $props();

  /** How many characters of an id a row draws, as the terminal draws them. */
  const SHORT = 8;

  let outcome = $state<CopyOutcome>('ready');

  // A new occupant is a new id, and `copied` was about the old one: the
  // clipboard then holds a string this row no longer shows, so the control
  // goes back to saying what it is rather than vouching for the other id.
  const initial = () => id;
  let drawn = $state(initial());
  $effect(() => {
    if (drawn !== id) {
      drawn = id;
      outcome = 'ready';
    }
  });

  function copy(): void {
    if (typeof navigator === 'undefined' || navigator.clipboard === undefined) {
      outcome = 'no-clipboard';
      return;
    }
    navigator.clipboard.writeText(id).then(
      () => {
        outcome = 'copied';
      },
      () => {
        outcome = 'failed';
      },
    );
  }
</script>

<span class="sid">
  <span class="fk">session</span>
  <span class="id" title={id}>{id.slice(0, SHORT)}</span>
  <button
    class="cp"
    type="button"
    title={copyReason(outcome)}
    aria-label={copyReason(outcome)}
    onclick={copy}
  >
    {copyLabel(outcome)}
  </button>
</span>

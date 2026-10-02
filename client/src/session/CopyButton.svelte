<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { copyReason, type CopyOutcome } from './view';

  /**
   * The copy control for a session id, which the header's row and its folded
   * panel both draw.
   *
   * **Both ways the write can fail say so on the control** rather than passing
   * as a click that worked: a page over a plain-http origin has no
   * `navigator.clipboard` at all, and a live one can still refuse the write.
   * Which of the two it was is the difference between changing the page's
   * origin and changing a permission, so the reason names it, on the title and
   * the accessible name.
   *
   * The control draws a MARK rather than a word - the copy glyph, a check once
   * the write landed, an x when it could not - so the id reads as data rather
   * than the pair reading as a form.
   */
  let { id }: { id: string } = $props();

  let outcome = $state<CopyOutcome>('ready');

  // A new occupant is a new id, and `copied` was about the old one: the
  // clipboard then holds a string this row no longer shows, so the control
  // goes back to saying what it is rather than vouching for the other id.
  const initial = () => id;
  let drawn = $state(initial());
  $effect(() => {
    if (drawn !== id) {
      drawn = id;
      clearSettle();
      outcome = 'ready';
    }
  });

  /** How long a landed write vouches for itself before the mark goes back. */
  const SETTLE_MS = 2_500;
  let settle: ReturnType<typeof setTimeout> | null = null;

  function clearSettle(): void {
    if (settle !== null) {
      clearTimeout(settle);
      settle = null;
    }
  }

  // A check that never clears reads as a state of the row rather than as the
  // answer to a click, so it goes back to what the control is - the same seat
  // or a new one - once the reader has had a beat to see it.
  $effect(() => () => clearSettle());

  function copy(): void {
    // The id the write was issued for. A write can settle after an occupant
    // swap has put a new id on the row, and a resolve that does not match the
    // id here now must change nothing: it is about a string this row no longer
    // shows.
    const issued = id;
    if (typeof navigator === 'undefined' || navigator.clipboard === undefined) {
      outcome = 'no-clipboard';
      return;
    }
    navigator.clipboard.writeText(issued).then(
      () => {
        if (issued !== id) return;
        outcome = 'copied';
        clearSettle();
        settle = setTimeout(() => {
          settle = null;
          outcome = 'ready';
        }, SETTLE_MS);
      },
      () => {
        if (issued === id) outcome = 'failed';
      },
    );
  }

  /** The mark the outcome draws: what the click does, then what it did. */
  const mark = $derived(outcome === 'copied' ? 'check' : outcome === 'ready' ? 'copy' : 'x');
  const tone = $derived(outcome === 'copied' ? 'ok' : outcome === 'ready' ? '' : 'bad');
</script>

<button
  class="cp {tone}"
  type="button"
  title={copyReason(outcome)}
  aria-label={copyReason(outcome)}
  onclick={copy}
>
  <Icon name={mark} />
</button>

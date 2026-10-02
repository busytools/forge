<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Echo as Pending } from './echoes.svelte';
  import Prose from './Prose.svelte';

  /**
   * The reader's own words, from the enter that sent them to the core's own copy.
   *
   * The same row the words will occupy once they land, which is the whole point:
   * the words do not move, one mark stops saying what it was saying. A refusal
   * keeps them here with the reason and a retry rather than handing them back to
   * the box, where they would land over whatever was typed since.
   */
  let { echo, onretry }: { echo: Pending; onretry: () => void } = $props();

  /** How long the mark holds once the core has the words. The composer's own beat, the same length. */
  const BEAT_MS = 450;

  /**
   * Whether the mark still draws, which outlives the send it describes.
   *
   * **A beat, because the answer is usually instant.** A local round trip takes
   * tens of milliseconds, so the whole of "still not sent" would be one frame of
   * flash, and a state a reader never sees is not a state they were told about.
   * The words are on screen throughout either way - only the mark is held.
   */
  // svelte-ignore state_referenced_locally
  let marked = $state(echo.state === 'sending');
  let timer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    if (echo.state === 'sending') {
      marked = true;
      return;
    }
    // Only a send this component watched being taken gets the beat: one that
    // arrives already taken was taken while the reader was somewhere else.
    if (echo.state !== 'taken' || !marked) return;
    if (timer !== null) clearTimeout(timer);
    // Held in a variable rather than returned as this effect's cleanup, which
    // runs before every re-run and would cancel the beat on the next frame.
    timer = setTimeout(() => {
      marked = false;
      timer = null;
    }, BEAT_MS);
  });

  $effect(() => () => {
    if (timer !== null) clearTimeout(timer);
  });
</script>

<div class="mine" class:bad={echo.state === 'failed'}>
  <Prose text={echo.words} preserveLines />
  {#if echo.state === 'failed'}
    <div class="why">
      <Icon name="x" class="ic" />not sent · {echo.why}
      <button class="retry" type="button" onclick={onretry}>retry</button>
    </div>
  {:else if marked}
    <!-- The rule above is unchanged while this shows: it means the words are
         the reader's, and a mark that also meant "in flight" would be two
         meanings on one thing. -->
    <div class="st"><span class="ring"></span>sending</div>
  {/if}
</div>

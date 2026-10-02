<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Echo as Pending } from './echoes.svelte';
  import Prose from './Prose.svelte';

  /**
   * The reader's own words, drawn before the core has them.
   *
   * The same row the words will occupy once they land, which is the whole
   * point: nothing moves on screen when the send is taken, one mark stops
   * saying so. A send that fails keeps its words here rather than handing them
   * back to the box, so words typed since are never landed over.
   */
  let { echo, onretry }: { echo: Pending; onretry: () => void } = $props();
</script>

<div class="mine" class:bad={echo.state === 'failed'}>
  <Prose text={echo.words} preserveLines />
  {#if echo.state === 'sending'}
    <!-- The rule above is unchanged while this shows: it means the words are
         the reader's, and a mark that also meant "in flight" would be two
         meanings on one thing. -->
    <div class="st"><span class="ring"></span>sending</div>
  {:else}
    <div class="why">
      <Icon name="x" class="ic" />not sent · {echo.why}
      <button class="retry" type="button" onclick={onretry}>retry</button>
    </div>
  {/if}
</div>

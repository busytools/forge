<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { joinedLine } from './text';

  /**
   * One message forge's own machinery dispatched: a chase nudge, a browser
   * warning, a kick.
   *
   * **A nudge is a row of the work, not the reader's words.** Nobody typed it,
   * so it leads with the board's own mark and never wears the reader's rule -
   * the same drawing for every automated nudge, board and browser alike. A
   * long one - a user-wait nudge, a browser sentence - opens on the whole
   * of what it said, the way a thought row does.
   */
  let { text, open = false }: { text: string; open?: boolean } = $props();

  /** Whether the row is open; a closed row carries its summary and nothing else. */
  let opened = $state(untrack(() => open));
</script>

<details class="leaf nudge" bind:open={opened}>
  <summary>
    <Icon name="tasks" class="gl" />
    <!--
      The row's rendered line, which the module produced from escaped input:
      same renderer as the body, raw HTML off.
    -->
    <!-- eslint-disable-next-line svelte/no-at-html-tags -->
    <span class="tn">{@html renderInlineProse(joinedLine(text))}</span>
    <Chevron />
  </summary>
  {#if opened}
    <div class="body">
      <Prose {text} />
    </div>
  {/if}
</details>

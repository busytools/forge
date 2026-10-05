<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';
  import { renderInlineProse } from './prose';
  import { joinedLine } from './text';

  /**
   * One thing the model thought, as a row of the work.
   *
   * **One drawing, two folds.** The session's own fold draws a thought row in
   * a turn's work and a dispatch row draws its instance's thinking in its
   * expansion; a second copy here would be the same row kept in step by hand.
   * The body is markdown - the model writes its reasoning in headings, lists
   * and code, and the terminal has no row for it at all - and the summary
   * line is that markdown rendered inline, from the same renderer and the
   * same escaping as the body, so a row and its open read the marks alike.
   */
  let { text }: { text: string } = $props();
</script>

<details class="leaf">
  <summary>
    <Icon name="message-circle-more" class="gl" />
    <!--
      The row's rendered line, which the module produced from escaped input:
      same renderer as the body, raw HTML off.
    -->
    <!-- eslint-disable-next-line svelte/no-at-html-tags -->
    <span class="tn">{@html renderInlineProse(joinedLine(text))}</span>
    <Chevron />
  </summary>
  <div class="body">
    <Prose {text} />
  </div>
</details>

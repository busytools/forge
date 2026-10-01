<script lang="ts">
  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import { firstLine, paragraphs } from './text';

  /**
   * What the model thought, on the row it leaves behind.
   *
   * **The terminal does not draw this**, and the row says so rather than
   * pretending to be parity: its own arm for a thinking block sets a running
   * status and traces a character count, while the only figure the turn's row
   * shows is estimated tokens - `thinking 1.2k` on the running row, the full
   * `est` cell in the expanded body. So this is the client going beyond it -
   * and it exists because the words are ON THE WIRE and dropping them is the
   * one thing a frame may not be. The shape is the terminal's own vocabulary
   * for a block nothing else shows: collapsed, carrying the first of its
   * words, with the whole of it behind the row's own open.
   */
  let { text }: { text: string } = $props();
</script>

<details class="think">
  <summary>
    <!-- The kind, ahead of the words. Every other row in the column says what
         it IS - a tool row carries its tool, a notice its severity - and this
         one opened on a sentence, so its kind had to be inferred from its
         content. It is also the disclosure's accessible name: a screen reader
         is given the summary's text and nothing else. -->
    <span class="tkind"><Icon name="think" /><span class="word">thinking</span></span>
    <span class="tn">{firstLine(text)}</span>
    <Chevron />
  </summary>
  <div class="body">
    <div class="pbody">
      {#each paragraphs(text) as paragraph, at (at)}
        <p>{paragraph}</p>
      {/each}
    </div>
  </div>
</details>

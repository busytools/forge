<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { omittedText } from './leaves';
  import type { AnsweredQuestion } from './units';

  /**
   * The question the assistant asked, with whatever the reader answered.
   *
   * **The marks are the sprite's, not a character cell's.** The card led its
   * answers with a right arrow because the view this client replaced had
   * nothing else to draw with, and it reads as punctuation beside the icons on
   * every other row.
   *
   * `omitted` is the size of an answers record the served page left out
   * (#2374): the question keeps the glyph and the words it asked, and the
   * marker under it names what is missing in the diff's own marks line rather
   * than a second kind of drawing.
   */
  let { asked = [], omitted = null }: { asked?: AnsweredQuestion[]; omitted?: number | null } =
    $props();
</script>

{#each asked as pair, at (at)}
  <div class="card">
    <div class="q">
      <Icon name="question" class="qm" />
      {pair.question}
    </div>
    {#each pair.picked_labels as picked, at (at)}
      <div class="a">
        <Icon name="check" class="am" />
        <span class="picked">{picked}</span>
      </div>
    {/each}
    {#if pair.typed_note !== null}
      <div class="a">
        <!-- The typed row holds the slot the picked rows mark in - empty - so
             its label starts in the column the picked labels do rather than a
             step to their left. -->
        <span class="gap"></span>
        <span class="am">you typed:</span>
        <span class="typed">{pair.typed_note}</span>
      </div>
    {/if}
  </div>
{/each}
{#if omitted !== null}
  <div class="patchline"><span class="patchmark">{omittedText(omitted)}</span></div>
{/if}

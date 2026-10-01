<script lang="ts">
  /**
   * What the model thought, on the row it leaves behind.
   *
   * **The terminal does not draw this**, and the row says so rather than
   * pretending to be parity: its own arm for a thinking block sets a running
   * status and traces a character count, and the only figure it shows is
   * `thinking N est` in the stats row. So this is the client going beyond it -
   * and it exists because the words are ON THE WIRE and dropping them is the
   * one thing a frame may not be. The shape is the terminal's own vocabulary
   * for a block nothing else shows: collapsed, carrying the first of its words,
   * with the whole of it behind the row's own open.
   */
  let { text }: { text: string } = $props();

  let opened = $state(false);

  /** The first line that says anything, which is what the summary carries. */
  const head = $derived(
    text
      .split('\n')
      .find((line) => line.trim() !== '')
      ?.trim() ?? '',
  );

  /** A body's paragraphs, which are the blank-line breaks the prose arrives with. */
  function paragraphs(body: string): string[] {
    return body
      .split('\n\n')
      .map((one) => one.trim())
      .filter((one) => one !== '');
  }
</script>

<details class="think" bind:open={opened}>
  <summary>
    <span class="tn">{head}</span>
    <span class="tog">{opened ? 'collapse' : 'expand'}</span>
  </summary>
  <div class="body">
    <div class="pbody">
      {#each paragraphs(text) as paragraph, at (at)}
        <p>{paragraph}</p>
      {/each}
    </div>
  </div>
</details>

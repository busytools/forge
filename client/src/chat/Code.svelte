<script lang="ts">
  import { whenVisible } from './visible';

  /**
   * A source file a call read, as a panel with its grammar on the header and
   * the file under it, coloured by shiki.
   *
   * **The grammar is handed in, never re-detected.** The row reads the
   * format from the file with the CLI's line-number gutter off, because a
   * shebang has to be read from the file's first line; re-detecting here on
   * the gutter-numbered text would read no shebang and draw it plain.
   *
   * **Plain first, coloured when the grammar lands.** The escaped text draws
   * synchronously, so a row or a server render never waits on a grammar; the
   * shiki pass replaces it a moment later. **The colour also waits for the
   * viewport**: a panel nobody can see is not tokenised until it scrolls in,
   * because a heavy turn mounts every one of its panels at once. `language`
   * of `null` draws with no language in the header and a caption saying why.
   */
  let { language, text }: { language: string | null; text: string } = $props();

  /** The coloured body, once the grammar has loaded and the panel was seen; `null` is the plain one. */
  let body = $state<string | null>(null);

  /** The panel's own element, which is what the viewport observer watches. */
  let panel = $state<HTMLElement | null>(null);

  $effect(() => {
    const id = language;
    const source = text;
    const node = panel;
    body = null;
    if (id === null || node === null) return;
    let live = true;
    // A grammar that cannot load leaves the escaped panel, which already
    // carries every word: a colouring failure is not a lost file.
    const stop = whenVisible(node, () => {
      void import('../format/highlight')
        .then(async ({ highlightedBody }) => {
          const drawn = await highlightedBody(id, source);
          if (live) body = drawn;
        })
        .catch(() => undefined);
    });
    return () => {
      live = false;
      stop();
    };
  });
</script>

<div class="code panel" bind:this={panel}>
  {#if language !== null}<div class="lang">{language}</div>{/if}
  {#if body !== null}
    <!-- eslint-disable-next-line svelte/no-at-html-tags -->
    <pre>{@html body}</pre>
  {:else}
    <pre>{text}</pre>
  {/if}
</div>
{#if language === null}
  <div class="code-note">plain text - no syntax for this file</div>
{/if}

<script lang="ts">
  import { panelHtml } from './code';

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
   * shiki pass replaces it a moment later. `language` of `null` draws with no
   * language in the header and a caption saying why.
   */
  let { language, text }: { language: string | null; text: string } = $props();

  /** The highlighted panel, once the grammar has loaded; `null` is the plain one. */
  let html = $state<string | null>(null);

  $effect(() => {
    const id = language;
    const body = text;
    html = null;
    if (id === null) return;
    let live = true;
    // A grammar that cannot load leaves the escaped panel, which already
    // carries every word: a colouring failure is not a lost file.
    void import('../format/highlight')
      .then(async ({ highlightedBody }) => {
        const drawn = await highlightedBody(id, body);
        if (live) html = panelHtml(id, drawn);
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  });
</script>

{#if html !== null}
  <!--
    The highlighter's own escaped output: it escapes the text it is given
    whether or not it has a grammar for it.
  -->
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  {@html html}
{:else}
  <div class="code">
    {#if language !== null}<div class="lang">{language}</div>{/if}
    <pre>{text}</pre>
  </div>
{/if}
{#if language === null}
  <div class="code-note">plain text - no syntax for this file</div>
{/if}

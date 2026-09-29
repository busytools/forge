<script lang="ts">
  import { languageOf, renderCode } from './code';

  /**
   * A source file a call read, as the mockup draws it: a card with the
   * language on its header and the file under it, coloured.
   *
   * `path` is what names the language, and a path this page has no language
   * for draws the file as text rather than guessing: colour a reader trusts
   * more than they should, so a wrong language is worse than none.
   */
  let { path, text }: { path: string; text: string } = $props();

  const language = $derived(languageOf(path));
  const html = $derived(renderCode(language, text));
</script>

<div class="code">
  <div class="lang">{language ?? 'text'}</div>
  <!--
    The highlighter's own escaped output. It escapes the text it is given
    whether or not it has a grammar for it, which is why the uncoloured path
    goes through it too rather than being spliced in raw.
  -->
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  <pre>{@html html}</pre>
</div>

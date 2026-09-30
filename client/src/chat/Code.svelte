<script lang="ts">
  import { codePanel, languageOf } from './code';

  /**
   * A source file a call read, as the mockup draws it: a panel with the
   * language on its header and the file under it, coloured.
   *
   * `path` is what names the language, and a path this page has no language
   * for draws the file as text rather than guessing: colour a reader trusts
   * more than they should, so a wrong language is worse than none.
   *
   * The panel itself is built beside the highlighter, because a message's
   * fenced block draws the same one and only one of the two can be a component.
   */
  let { path, text }: { path: string; text: string } = $props();

  const language = $derived(languageOf(path));
  const html = $derived(codePanel(language ?? 'text', language, text));
</script>

<!--
  The highlighter's own escaped output. It escapes the text it is given whether
  or not it has a grammar for it, which is why the uncoloured path goes through
  it too rather than being spliced in raw.
-->
<!-- eslint-disable-next-line svelte/no-at-html-tags -->
{@html html}

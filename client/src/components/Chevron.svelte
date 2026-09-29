<script lang="ts">
  import Icon from './Icon.svelte';

  /**
   * The disclosure arrow every `<details>` on the page carries. One chevron,
   * turned by the open state rather than swapped for a second glyph.
   */
  let { class: klass = '' }: { class?: string } = $props();

  const css = $derived(klass === '' ? 'arw' : `arw ${klass}`);
</script>

<Icon name="chev" class={css} />

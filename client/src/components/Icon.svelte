<script lang="ts">
  /**
   * One icon. `name` is a sprite id without its `i-` prefix, and `class`
   * carries what the page's own rules colour and size it by.
   */
  let { name, class: klass = '' }: { name: string; class?: string } = $props();

  const css = $derived(klass === '' ? 'ic' : `ic ${klass}`);
</script>

<svg class={css}><use href={`#i-${name}`}></use></svg>

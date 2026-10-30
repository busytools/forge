<script lang="ts">
  /**
   * One icon. `name` is a sprite id without its `i-` prefix, and `class`
   * carries what the page's own rules colour and size it by. A `label` names
   * the icon for a reader who cannot see it, which makes it an image rather
   * than decoration.
   */
  let {
    name,
    class: klass = '',
    label = undefined,
  }: { name: string; class?: string; label?: string } = $props();

  const css = $derived(klass === '' ? 'ic' : `ic ${klass}`);
</script>

<svg class={css} role={label === undefined ? undefined : 'img'} aria-label={label}>
  <use href={`#i-${name}`}></use>
</svg>

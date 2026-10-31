<script lang="ts">
  /** One boolean, as a switch. The label is the row's own key text. */
  let {
    checked,
    label,
    onchange,
  }: { checked: boolean; label: string; onchange: (next: boolean) => void } = $props();
</script>

<button
  type="button"
  class="toggle"
  class:on={checked}
  role="switch"
  aria-checked={checked}
  aria-label={label}
  onclick={() => onchange(!checked)}
>
  <span class="track" aria-hidden="true"><span class="thumb"></span></span>
  <span class="word">{checked ? 'on' : 'off'}</span>
</button>

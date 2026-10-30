<script lang="ts">
  /** One whole number. An empty box is `null`, which unsets an optional key. */
  let {
    value,
    label,
    onchange,
  }: { value: number | null; label: string; onchange: (next: number | null) => void } = $props();
</script>

<input
  class="number-field"
  data-editor="settings"
  type="number"
  step="1"
  aria-label={label}
  value={value ?? ''}
  oninput={(event) => {
    const text = event.currentTarget.value.trim();
    if (text === '') return onchange(null);
    const parsed = Number(text);
    onchange(Number.isFinite(parsed) ? Math.trunc(parsed) : null);
  }}
/>

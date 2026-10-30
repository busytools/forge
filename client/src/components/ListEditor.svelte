<script lang="ts">
  import Field from '../composer/Field.svelte';

  /** A list of strings: one box per item, and a press to add another. */
  let {
    values,
    label,
    onchange,
  }: { values: string[]; label: string; onchange: (next: string[]) => void } = $props();

  function replace(at: number, text: string): void {
    const next = [...values];
    next[at] = text;
    onchange(next);
  }
</script>

<div class="list-editor" role="group" aria-label={label}>
  {#each values as item, index (index)}
    <span class="list-item">
      <Field
        editor="settings"
        element="input"
        value={item}
        aria={{ label: `${label} item ${index + 1}` }}
        oninput={(event: Event) => replace(index, (event.currentTarget as HTMLInputElement).value)}
      />
      <button
        type="button"
        class="list-x"
        aria-label={`remove ${label} item ${index + 1}`}
        onclick={() => onchange(values.filter((_, at) => at !== index))}
      >
        &times;
      </button>
    </span>
  {/each}
  <button type="button" class="list-add" onclick={() => onchange([...values, ''])}>add</button>
</div>

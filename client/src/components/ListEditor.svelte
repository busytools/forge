<script lang="ts">
  /** A list of strings: one box per item, and a press to add another. */
  let {
    values,
    label,
    editor,
    onchange,
  }: {
    values: string[];
    label: string;
    editor: string;
    onchange: (next: string[]) => void;
  } = $props();

  function replace(at: number, text: string): void {
    const next = [...values];
    next[at] = text;
    onchange(next);
  }
</script>

<div class="list-editor" role="group" aria-label={label}>
  {#each values as item, index (index)}
    <span class="list-item">
      <input
        data-editor={editor}
        value={item}
        aria-label={`${label} item ${index + 1}`}
        oninput={(event) => replace(index, event.currentTarget.value)}
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

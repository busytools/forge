<script lang="ts">
  import Field from './Field.svelte';

  /**
   * The send-back line a held card opens when the reader wants to hand it to
   * the agent with words. Slack's draft and the vault's held call share it:
   * the shape is one row of words and the button that sends them, and two
   * copies of it would drift.
   */
  let {
    value = $bindable(),
    placeholder = 'What should change?',
    label,
    onkeydown,
    field,
    onsend,
  }: {
    value: string;
    placeholder?: string;
    label: string;
    onkeydown: (event: KeyboardEvent) => void;
    field: (el: HTMLElement | null) => void;
    onsend: () => void;
  } = $props();
</script>

<div class="sendback">
  <Field
    editor="dock"
    class="notes"
    bind:value
    rows={2}
    {placeholder}
    aria={{ label }}
    {onkeydown}
    {field}
  />
  <button class="btn" disabled={value.trim() === ''} onclick={onsend}>Send back</button>
</div>

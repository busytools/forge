<script lang="ts">
  /**
   * The files page's words box: the field, the acts, and nothing else.
   *
   * **No mic, by the drawing**: the approved mockup carries no dictation
   * control on this page, unlike the review page's boxes - so this is its
   * own small box over the composer's shared `Field`, not `WordsBox`. If
   * the mic is ever wanted here, `WordsBox` takes the page's `Voice` and
   * this component is what it replaces.
   *
   * **The words live on the PAGE, handed in as a shelf** (`{draft, busy}`):
   * the reader is re-mounted under a box whenever a thread grows a reply
   * row, and a draft this component owned would go with it - the same
   * arrangement the review page's boxes have.
   */
  import Field from '../composer/Field.svelte';
  import type { Shelf } from './comments';

  let {
    class: className = 'rv-thread compact',
    headline = null,
    placeholder,
    label,
    rows = 3,
    sendLabel,
    box,
    onsend,
    oncancel,
    field = () => {},
  }: {
    /** The card the box draws as, which is the page's own shape for it. */
    class?: string;
    /** The line above the box, when the box is anchored to something. */
    headline?: string | null;
    placeholder: string;
    /** The box's own name, for a surface whose placeholder is not one. */
    label: string;
    rows?: number;
    sendLabel: string;
    /** The page's shelf: the words and the write in flight. */
    box: Shelf;
    onsend: (text: string) => void;
    oncancel: () => void;
    /** The field, so the page can put the keyboard back in it. */
    field?: (el: HTMLElement | null) => void;
  } = $props();

  function send(): void {
    if (box.busy || box.draft.trim() === '') return;
    onsend(box.draft.trim());
  }
</script>

<article class={className}>
  {#if headline !== null}
    <div class="rv-tops">
      <span class="rv-where">{headline}</span>
    </div>
  {/if}
  <Field
    editor="nowhere"
    class="rv-txarea"
    {rows}
    {placeholder}
    aria={{ label }}
    bind:value={box.draft}
    onkeydown={(event: KeyboardEvent) => {
      // **Enter is the box's own action**, as everywhere else: Shift+Enter
      // keeps the newline, and an empty or busy box ignores it exactly as
      // its button does.
      if (event.key !== 'Enter' || event.shiftKey) return;
      event.preventDefault();
      send();
    }}
    {field}
  />
  <div class="rv-acts">
    <button
      class="rv-a on"
      type="button"
      disabled={box.busy || box.draft.trim() === ''}
      onclick={send}>{sendLabel}</button
    >
    <button class="rv-a" type="button" onclick={oncancel}>cancel</button>
  </div>
</article>

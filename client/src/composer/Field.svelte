<script lang="ts">
  import type { Editor } from './editors';

  /**
   * The one text box in the client.
   *
   * Its editor is a prop rather than a convention, because the routing that
   * decides the keyboard reads it - and a box that cannot say which editor it
   * is cannot be routed to.
   *
   * Everything a surface needs of its own element stays a prop: its class,
   * because the sheet is the sheet's business; its element kind, because the
   * connect screen connects on Enter and its own rules match `input`; and the
   * attributes a label, an error state or a list needs to reach it.
   */
  let {
    editor,
    value = $bindable(),
    placeholder,
    element = 'textarea',
    class: className = '',
    aria = {},
    rows,
    id,
    name,
    autocapitalize,
    oninput,
    onkeydown,
    field,
  }: {
    editor: Editor;
    value: string;
    placeholder?: string;
    element?: 'textarea' | 'input';
    class?: string;
    aria?: {
      controls?: string | undefined;
      activeDescendant?: string | undefined;
      autocomplete?: 'none' | 'list' | 'both' | 'inline' | undefined;
      invalid?: boolean | undefined;
      describedBy?: string | undefined;
    };
    rows?: number;
    id?: string;
    name?: string;
    autocapitalize?: 'off' | 'none' | 'on' | 'characters' | 'sentences' | 'words' | undefined;
    oninput?: (event: Event) => void;
    onkeydown?: (event: KeyboardEvent) => void;
    field?: (el: HTMLElement | null) => void;
  } = $props();

  let node = $state<HTMLElement | null>(null);

  // Through an effect rather than `bind:this`, because the handle is a callback
  // the surface keeps rather than a variable this component owns.
  $effect(() => {
    field?.(node);
  });
</script>

{#if element === 'input'}
  <input
    class={className}
    data-editor={editor}
    type="text"
    {placeholder}
    {id}
    {name}
    {autocapitalize}
    autocomplete="off"
    spellcheck="false"
    aria-autocomplete={aria.autocomplete}
    aria-controls={aria.controls}
    aria-activedescendant={aria.activeDescendant}
    aria-invalid={aria.invalid}
    aria-describedby={aria.describedBy}
    bind:this={node}
    bind:value
    {oninput}
    {onkeydown}
  />
{:else}
  <textarea
    class={className}
    data-editor={editor}
    {placeholder}
    {id}
    {name}
    {rows}
    autocomplete="off"
    spellcheck="false"
    aria-autocomplete={aria.autocomplete}
    aria-controls={aria.controls}
    aria-activedescendant={aria.activeDescendant}
    aria-invalid={aria.invalid}
    aria-describedby={aria.describedBy}
    bind:this={node}
    bind:value
    {oninput}
    {onkeydown}></textarea>
{/if}

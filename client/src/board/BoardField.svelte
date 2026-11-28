<script lang="ts">
  /**
   * One text box the board draws, with the composer's own dictation beside it.
   *
   * **Every text box a client draws carries a take** (the house rule): this is
   * the field, the mic, the take's bar in the field's place while one runs,
   * and the line a failed attempt leaves - the same control, the same card and
   * the same landing model the composer and the review page use. The page
   * owns the take; this component names its box and hands the page its own
   * setter.
   *
   * A box drawn with no dictation (a page with no connection, a test) is the
   * plain input it was: the mic is offered only where a take can run.
   */
  import DictateControl from '../composer/DictateControl.svelte';
  import TakeCard from '../composer/TakeCard.svelte';
  import type { BoardVoice } from './voice';

  let {
    id,
    value = '',
    placeholder,
    label,
    voice = null,
    oninput = null,
    onkeydown = null,
  }: {
    /** The box's identity, the key its take's landing is placed by. */
    id: string;
    value?: string;
    placeholder: string;
    label: string;
    voice?: BoardVoice | null;
    /** The words, on every change - a landing included. */
    oninput?: ((text: string) => void) | null;
    onkeydown?: ((event: KeyboardEvent) => void) | null;
  } = $props();

  let field = $state<HTMLInputElement | null>(null);

  /**
   * The field's own listener, attached to the node rather than delegated:
   * Svelte's delegated `oninput` runs at the root, where a programmatically
   * dispatched event that does not bubble never arrives.
   */
  function typed(node: HTMLInputElement): { destroy: () => void } {
    const on = (): void => oninput?.(node.value);
    node.addEventListener('input', on);
    return { destroy: () => node.removeEventListener('input', on) };
  }

  /** The words this box dictated land back here, and the caret follows. */
  $effect(() => {
    if (voice === null) return;
    return voice.register(id, {
      set: (text: string) => {
        oninput?.(text);
        field?.focus();
      },
      focus: () => field?.focus(),
    });
  });
</script>

<div class="bf">
  {#if voice !== null && voice.activeId === id && voice.phase !== null}
    <TakeCard
      take={voice.take}
      wire={voice.wire}
      phase={voice.phase}
      silent={voice.silent}
      oncancel={voice.cancel}
      spread
    />
  {:else}
    <input
      class="b-in w-note"
      data-editor="board"
      type="text"
      {placeholder}
      aria-label={label}
      value={value ?? ''}
      bind:this={field}
      use:typed
      onkeydown={onkeydown ?? undefined}
    />
  {/if}
  {#if voice !== null}
    <DictateControl dictate={voice.dictate} onpress={() => voice.press(id)} holdable={false} />
  {/if}
  {#if voice !== null && voice.line !== null && voice.lineId === id}
    <span class="bf-line">{voice.line}</span>
  {/if}
</div>

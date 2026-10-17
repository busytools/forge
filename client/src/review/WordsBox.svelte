<script lang="ts">
  /**
   * A box in the review page that takes the reader's words: the field, the
   * acts, and the mic - over the take the page owns.
   *
   * One component for both halves' boxes - the comment on a diff line and the
   * reply under a thread - because they are the same thing: a labelled box
   * whose primary act files the words, with the composer's mic control at the
   * far end and the take's own bar in the field's place while it runs.
   *
   * **The words are NOT held here**: the canvas rebuilds a row whenever it
   * moves, and a draft this component owned would go with it - the page holds
   * it on the box it hands in, and reads the same box back. The take's setup
   * is the page's too: one microphone, one take, one landing, however many
   * boxes are on screen to wear a mic.
   */
  import DictateControl from '../composer/DictateControl.svelte';
  import DictationPanel from '../composer/DictationPanel.svelte';
  import Field from '../composer/Field.svelte';
  import TakeCard from '../composer/TakeCard.svelte';
  import type { Voice, Words } from './boxes';

  let {
    class: className = 'rv-thread compact',
    headline = null,
    placeholder,
    label,
    rows = 3,
    sendLabel,
    box,
    voice,
    onsend,
    oncancel,
    onpress = () => {},
    onhold = () => {},
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
    /** The primary act's own word: `comment`, or `send`. */
    sendLabel: string;
    /** The page's box: the words, the write in flight, the take's line, and
     *  where the take is - read through the proxy, so a take starting under
     *  the reader redraws the bar. */
    box: Words;
    /** What dictation means to this page, shared by every box it draws. */
    voice: Voice;
    onsend: (text: string) => void;
    oncancel: () => void;
    /** The reader's last act was on this box's mic, which is what keeps the caret on it. */
    onpress?: () => void;
    /** The settings were opened from this box's mic, which is what anchors the panel here. */
    onhold?: () => void;
    /** The field, so the page can put the keyboard back in it. */
    field?: (el: HTMLElement | null) => void;
  } = $props();

  /** This box's own field, which is where the panel's close puts the keyboard. */
  let own = $state<HTMLElement | null>(null);

  function send(): void {
    if (box.busy || box.draft.trim() === '') return;
    onsend(box.draft.trim());
  }

  /**
   * **The settings panel goes with the box that drew it.** The row is
   * mounted, dropped and mounted again as the page draws (a folded file, a
   * scope switch, the mode), and a panel left open behind it is a panel with
   * no box under it - nothing on the page can close what no card draws. Only
   * the box the panel was opened from closes it: another box's teardown must
   * not take a panel it is not drawing.
   */
  $effect(() => {
    return () => {
      if (box.panel) voice.dictate.panel = false;
    };
  });
</script>

<article class={className}>
  {#if headline !== null}
    <div class="rv-tops">
      <span class="rv-where">{headline}</span>
    </div>
  {/if}
  <!-- **The take takes the box's own line.** The row the words would be typed
       into is the row they arrive on, so nothing is added and nothing moves
       when a take starts or ends; the mic is the same control the composer
       wears, and the bar is the same card. -->
  <div class="line" class:takeline={box.phase !== null}>
    {#if box.phase !== null}
      <TakeCard
        take={box.take}
        wire={box.wire}
        phase={box.phase}
        silent={box.silent}
        oncancel={() => voice.dictate.act('cancel')}
        spread
      />
    {:else}
      <Field
        editor="nowhere"
        class="rv-txarea"
        {rows}
        {placeholder}
        aria={{ label }}
        bind:value={box.draft}
        onkeydown={(event: KeyboardEvent) => {
          // **Enter is the box's own action** (Ved, 2026-10-15): the words go
          // with the same key that sends them everywhere else, Shift+Enter
          // keeps the newline, and an empty or busy box ignores it exactly as
          // its button does.
          if (event.key !== 'Enter' || event.shiftKey) return;
          event.preventDefault();
          if (!box.busy && box.draft.trim() !== '') send();
        }}
        field={(el: HTMLElement | null) => {
          own = el;
          field(el);
        }}
      />
    {/if}
  </div>
  <div class="rv-acts">
    <button
      class="rv-a on"
      type="button"
      disabled={box.busy || box.draft.trim() === ''}
      onclick={send}>{sendLabel}</button
    >
    <button class="rv-a" type="button" onclick={oncancel}>cancel</button>
    {#if voice.offered}
      <DictateControl dictate={voice.dictate} {onpress} {onhold} />
    {/if}
  </div>
  {#if box.line !== null}
    <div class="rv-dictateline {box.line.tone}">{box.line.text}</div>
  {/if}
  {#if box.panel}
    <DictationPanel
      connection={voice.connection}
      axes={voice.axes}
      defaults={voice.defaults}
      bind={voice.bind}
      mode={voice.mode}
      device={voice.device}
      onaxes={voice.onaxes}
      ondevice={voice.ondevice}
      onclose={() => {
        voice.dictate.panel = false;
        // The keyboard goes back where Escape puts it, so closing by the
        // button is not a dead end for a keyboard reader.
        own?.focus();
      }}
    />
  {/if}
</article>

<style>
  /* The box is what a take's settings panel hangs off: the panel belongs to
     whichever box drew the mic that opened it, so the anchor travels with the
     box rather than with the page that draws one. */
  article {
    position: relative;
  }
</style>
